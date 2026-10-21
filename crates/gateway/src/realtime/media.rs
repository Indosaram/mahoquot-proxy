//! Owned ICE/DTLS/SRTP/SCTP media transport.
use std::sync::Arc;
use tokio::sync::{mpsc, watch};
use webrtc::api::{
    interceptor_registry::register_default_interceptors, media_engine::MediaEngine, APIBuilder,
};
use webrtc::data_channel::{data_channel_message::DataChannelMessage, RTCDataChannel};
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::{
    configuration::RTCConfiguration, sdp::session_description::RTCSessionDescription,
    RTCPeerConnection,
};
use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTPCodecType};
use webrtc::track::track_local::{
    track_local_static_rtp::TrackLocalStaticRTP, TrackLocal, TrackLocalWriter,
};

pub struct MediaRelay {
    pub downstream: Arc<RTCPeerConnection>,
    pub upstream: Arc<RTCPeerConnection>,
    closed: watch::Sender<bool>,
}

async fn peer() -> anyhow::Result<Arc<RTCPeerConnection>> {
    let mut engine = MediaEngine::default();
    engine.register_default_codecs()?;
    let registry = register_default_interceptors(Registry::new(), &mut engine)?;
    let mut settings = webrtc::api::setting_engine::SettingEngine::default();
    if crate::loopback_guard::is_qa_loopback_only() {
        settings.set_udp_network(webrtc::ice::udp_network::UDPNetwork::Ephemeral(
            webrtc::ice::udp_network::EphemeralUDP::new(18840, 18899)?,
        ));
        settings.set_include_loopback_candidate(true);
        settings.set_ip_filter(Box::new(|ip| ip.is_loopback()));
    }
    let api = APIBuilder::new()
        .with_media_engine(engine)
        .with_interceptor_registry(registry)
        .with_setting_engine(settings)
        .build();
    // No implicit STUN service or external provisioning. Host candidates only.
    Ok(Arc::new(
        api.new_peer_connection(RTCConfiguration::default()).await?,
    ))
}

impl MediaRelay {
    pub async fn offer(sdp: &str) -> anyhow::Result<(Arc<Self>, String)> {
        let downstream = peer().await?;
        let upstream = match peer().await {
            Ok(peer) => peer,
            Err(error) => {
                downstream.close().await?;
                return Err(error);
            }
        };
        let (closed, _) = watch::channel(false);
        let relay = Arc::new(Self {
            downstream,
            upstream,
            closed,
        });
        for peer in [&relay.downstream, &relay.upstream] {
            let closed = relay.closed.clone();
            peer.on_peer_connection_state_change(Box::new(move |state| {
                let closed = closed.clone();
                Box::pin(async move {
                    if matches!(state, webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::Failed
                        | webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::Closed) { closed.send_replace(true); }
                })
            }));
        }
        let result = relay.prepare(sdp).await;
        match result {
            Ok(offer) => Ok((relay, offer)),
            Err(error) => {
                relay.close().await;
                Err(error)
            }
        }
    }

    async fn prepare(self: &Arc<Self>, sdp: &str) -> anyhow::Result<String> {
        self.downstream
            .set_remote_description(RTCSessionDescription::offer(sdp.to_owned())?)
            .await?;
        let mut upstream_tracks = Vec::new();
        let mut downstream_tracks = Vec::new();
        for kind in [RTPCodecType::Audio, RTPCodecType::Video] {
            let (mime, rate, channels) = if kind == RTPCodecType::Audio {
                ("audio/opus", 48000, 2)
            } else {
                ("video/VP8", 90000, 0)
            };
            let codec = RTCRtpCodecCapability {
                mime_type: mime.into(),
                clock_rate: rate,
                channels,
                ..Default::default()
            };
            let to_up = Arc::new(TrackLocalStaticRTP::new(
                codec.clone(),
                mime.into(),
                "realtime".into(),
            ));
            let to_down = Arc::new(TrackLocalStaticRTP::new(
                codec,
                mime.into(),
                "realtime".into(),
            ));
            for (pc, track) in [(&self.upstream, &to_up), (&self.downstream, &to_down)] {
                let sender = pc
                    .add_track(track.clone() as Arc<dyn TrackLocal + Send + Sync>)
                    .await?;
                let mut done = self.closed.subscribe();
                tokio::spawn(async move {
                    let mut buffer = [0u8; 1500];
                    loop {
                        tokio::select! {
                            _ = done.changed() => break,
                            result = sender.read(&mut buffer) => if result.is_err() { break; },
                        }
                    }
                });
            }
            upstream_tracks.push((kind, to_up));
            downstream_tracks.push((kind, to_down));
        }
        self.install_tracks(upstream_tracks, downstream_tracks);
        let upstream = self
            .upstream
            .create_data_channel("oai-events", None)
            .await?;
        let (to_down, from_up) = mpsc::channel::<DataChannelMessage>(64);
        let from_up = Arc::new(tokio::sync::Mutex::new(Some(from_up)));
        let closed = self.closed.clone();
        upstream.on_message(Box::new(move |message| {
            let queue = to_down.clone();
            let closed = closed.clone();
            Box::pin(async move {
                if message.data.len() > 256 * 1024 || queue.try_send(message).is_err() {
                    closed.send_replace(true);
                }
            })
        }));
        let (to_up, from_down) = mpsc::channel::<DataChannelMessage>(64);
        pump(upstream, from_down, self.closed.clone());
        let closed = self.closed.clone();
        self.downstream.on_data_channel(Box::new(move |channel| {
            let queue = to_up.clone();
            let receiver = from_up.clone();
            let closed = closed.clone();
            Box::pin(async move {
                if channel.label() != "oai-events" {
                    let _ = channel.close().await;
                    return;
                }
                let Some(receiver) = receiver.lock().await.take() else {
                    let _ = channel.close().await;
                    return;
                };
                let signal = closed.clone();
                channel.on_message(Box::new(move |message| {
                    let queue = queue.clone();
                    let signal = signal.clone();
                    Box::pin(async move {
                        if message.data.len() > 256 * 1024 || queue.try_send(message).is_err() {
                            signal.send_replace(true);
                        }
                    })
                }));
                pump(channel, receiver, closed);
            })
        }));
        let mut gather = self.upstream.gathering_complete_promise().await;
        self.upstream
            .set_local_description(self.upstream.create_offer(None).await?)
            .await?;
        gather.recv().await;
        Ok(self
            .upstream
            .local_description()
            .await
            .ok_or_else(|| anyhow::anyhow!("missing upstream offer"))?
            .sdp)
    }

    fn install_tracks(
        &self,
        to_up: Vec<(RTPCodecType, Arc<TrackLocalStaticRTP>)>,
        to_down: Vec<(RTPCodecType, Arc<TrackLocalStaticRTP>)>,
    ) {
        for (source, target) in [(&self.downstream, to_up), (&self.upstream, to_down)] {
            let closed = self.closed.clone();
            source.on_track(Box::new(move |track, _, _| {
                let target = target.iter().find(|(kind, _)| *kind == track.kind()).map(|(_, target)| target.clone());
                let mut done = closed.subscribe();
                Box::pin(async move {
                    let Some(target) = target else { return; };
                    tokio::spawn(async move { loop { tokio::select! {
                        _ = done.changed() => break,
                        packet = track.read_rtp() => match packet {
                            Ok((packet, _)) => if target.write_rtp(&packet).await.is_err() { break; },
                            Err(_) => break,
                        }
                    }}});
                })
            }));
        }
    }

    pub async fn answer(&self, sdp: &str) -> anyhow::Result<String> {
        self.upstream
            .set_remote_description(RTCSessionDescription::answer(sdp.to_owned())?)
            .await?;
        let mut gather = self.downstream.gathering_complete_promise().await;
        self.downstream
            .set_local_description(self.downstream.create_answer(None).await?)
            .await?;
        gather.recv().await;
        Ok(self
            .downstream
            .local_description()
            .await
            .ok_or_else(|| anyhow::anyhow!("missing downstream answer"))?
            .sdp)
    }

    pub async fn close(&self) {
        self.closed.send_replace(true);
        let (a, b) = tokio::join!(self.downstream.close(), self.upstream.close());
        if let Err(error) = a {
            tracing::debug!(%error, "downstream media close");
        }
        if let Err(error) = b {
            tracing::debug!(%error, "upstream media close");
        }
    }
    pub fn cancellation(&self) -> watch::Receiver<bool> {
        self.closed.subscribe()
    }
}

fn pump(
    channel: Arc<RTCDataChannel>,
    mut receiver: mpsc::Receiver<DataChannelMessage>,
    closed: watch::Sender<bool>,
) {
    let (ready, mut opened) = watch::channel(false);
    channel.on_open(Box::new(move || {
        let ready = ready.clone();
        Box::pin(async move {
            let _ = ready.send_replace(true);
        })
    }));
    let writable = Arc::new(tokio::sync::Notify::new());
    let signal = writable.clone();
    let callback_channel = channel.clone();
    tokio::spawn(async move {
        callback_channel
            .on_buffered_amount_low(Box::new(move || {
                let signal = signal.clone();
                Box::pin(async move {
                    signal.notify_one();
                })
            }))
            .await;
    });
    let mut done = closed.subscribe();
    tokio::spawn(async move {
        channel.set_buffered_amount_low_threshold(512 * 1024).await;
        if !*opened.borrow() {
            tokio::select! { _ = opened.changed() => {}, _ = done.changed() => return }
        }
        loop {
            let message = tokio::select! { message = receiver.recv() => match message { Some(message) => message, None => break }, _ = done.changed() => break };
            let low = writable.notified();
            tokio::pin!(low);
            low.as_mut().enable();
            if channel.buffered_amount().await > 1024 * 1024 {
                tokio::select! { _ = low => {}, _ = done.changed() => break }
            }
            let result = if message.is_string {
                match std::str::from_utf8(&message.data) {
                    Ok(text) => channel.send_text(text).await,
                    Err(_) => {
                        closed.send_replace(true);
                        break;
                    }
                }
            } else {
                channel.send(&message.data).await
            };
            if result.is_err() {
                closed.send_replace(true);
                break;
            }
        }
    });
}

impl Drop for MediaRelay {
    fn drop(&mut self) {
        self.closed.send_replace(true);
        let downstream = self.downstream.clone();
        let upstream = self.upstream.clone();
        tokio::spawn(async move {
            let (a, b) = tokio::join!(downstream.close(), upstream.close());
            if let Err(error) = a {
                tracing::debug!(%error, "downstream media drop");
            }
            if let Err(error) = b {
                tracing::debug!(%error, "upstream media drop");
            }
        });
    }
}
