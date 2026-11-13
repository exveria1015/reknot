// These declarations are explicit runtime contracts, not inferred JS types.
const CONTRACT_VERSION = 1;
const LABEL = "rules-v1";
const POINTS = 100;
const ROUND_MS = 30000;
const TICK_MS = 200;

exports.contract = {
  version: CONTRACT_VERSION,
  state: { ticks: "nonnegative-integer" },
  answer: { correct: "choice", choice: "choice", delay_ms: "milliseconds" },
};
exports.info = (generation) => ({
  label: LABEL,
  correct_points: POINTS,
  round_ms: ROUND_MS,
  tick_ms: TICK_MS,
  generation,
});
exports.answer = async (input, context) => {
  if (input.delay_ms) await context.sleep(input.delay_ms);
  context.signal.throwIfAborted();
  const correct = input.correct === input.choice;
  return {
    correct,
    points: correct ? POINTS : 0,
    generation: context.generation,
    label: LABEL,
  };
};
exports.clock = async (context) => {
  while (!context.signal.aborted) {
    await context.sleep(TICK_MS);
    context.state.ticks++;
    context.publish({
      ticks: context.state.ticks,
      generation: context.generation,
      label: LABEL,
    });
  }
};
