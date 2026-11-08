import { test, expect } from 'bun:test';
import { balancedOrder, validateRounds } from './fair-translation.mjs';

test('balanced order gives both candidates each position equally', () => {
  const orders = Array.from({ length: 6 }, (_, i) => balancedOrder(i + 1, 35));
  expect(orders.filter(order => order[0] === 'B')).toHaveLength(3);
  expect(orders.filter(order => order[0] === 'C')).toHaveLength(3);
});
const sample = () => ({ total: 1, successful: 1, failed: 0, semantic_samples: 1, correctness_passed: true, ttft_ms: { p50: 1, p99: 1 }, ttfb_ms: [1], completion_ms: [2] });
test('missing paired round cannot produce verdict', () => {
  expect(() => validateRounds({ 's/1/B': sample() }, 2, ['s'])).toThrow('missing round');
});
test('wrong usage marked failed cannot produce timing verdict', () => {
  const runs = Object.fromEntries([1,2].flatMap(r => ['B','C'].map(t => [`s/${r}/${t}`, sample()])));
  runs['s/2/C'].correctness_passed = false;
  expect(() => validateRounds(runs, 2, ['s'])).toThrow('incorrect round');
});
test('threshold scales with complete balanced round count', () => {
  const runs = Object.fromEntries(Array.from({ length: 8 }, (_, i) => i + 1).flatMap(r => ['B','C'].map(t => [`s/${r}/${t}`, sample()])));
  expect(validateRounds(runs, 8, ['s']).threshold).toBe(7);
  expect(() => validateRounds(runs, 3, ['s'])).toThrow('position balance');
});
