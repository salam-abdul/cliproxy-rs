import type { Data } from "./core.ts";

const counter = (v: unknown): number | null =>
  typeof v === "number" && Number.isSafeInteger(v) && v >= 0 ? v : null;
const tokenRows = () =>
  ["Uncached input", "Output", "Cache read", "Cache write"].map((label) => ({
    label,
    tokens: null as number | null,
    reported: 0,
  }));
export const isFailure = (e: Data) =>
  !["success", "pending", "client_cancelled", "fallback_served"].includes(
    e.outcome,
  );
const outcomes: Record<string, string> = {
  success: "Succeeded",
  client_cancelled: "Cancelled by client",
  pending: "No final outcome recorded",
  overloaded_error: "Provider overloaded",
  rate_limit_error: "Provider rate limit reached",
  quota_unavailable: "No account has available quota",
  authentication_error: "Account sign-in rejected",
  transport_lost: "Connection lost",
  incomplete_stream: "Response interrupted",
  observation_incomplete: "Response observation incomplete",
  attempt_limit: "Account retry limit reached",
  provider_error: "Provider error",
};
const fallbackFailures: Record<string, string> = {
  unknown_model: "no provider serves it",
  provider_not_allowed: "its provider cannot serve a fallback",
  no_output: "it gave no answer",
};
/** Why a rejected request's quota fallback did not answer. */
export const fallbackNote = (e: Data) =>
  e.fallbackFailure && e.fallbackModel
    ? `Fallback to ${e.fallbackModel} failed: ${fallbackFailures[e.fallbackFailure] || e.fallbackFailure}`
    : undefined;
export const outcomeLabel = (e: Data) =>
  e.recovered
    ? "Recovered after account switch"
    : e.outcome === "fallback_served"
      ? `Fell back to ${e.fallbackModel || "another model"}`
      : Object.hasOwn(outcomes, e.outcome)
        ? outcomes[e.outcome]
        : String(e.outcome || "Unknown outcome").replaceAll("_", " ");

export function summarizeHistory(history: Data) {
  const buckets = Array.isArray(history.buckets)
    ? (history.buckets as Data[])
    : [];
  const models = Array.isArray(history.models)
    ? (history.models as Data[])
    : [];
  const tokens = tokenRows();
  const requests = buckets.reduce(
    (n, bucket) => n + (counter(bucket.requests) ?? 0),
    0,
  );
  const failures = buckets.reduce(
    (n, bucket) => n + (counter(bucket.failures) ?? 0),
    0,
  );
  for (const model of models) {
    const reported = [
      model.reportedInput,
      model.reportedOutput,
      model.reportedCacheRead,
      model.reportedCacheWrite,
    ];
    const counts = [
      model.inputTokens,
      model.outputTokens,
      model.cacheReadTokens,
      model.cacheWriteTokens,
    ];
    counts.forEach((value, kind) => {
      const n = counter(value);
      const observed = counter(reported[kind]);
      if (n === null || !observed) return;
      const row = tokens[kind];
      row.tokens = (row.tokens ?? 0) + n;
      row.reported += observed;
    });
  }
  return {
    buckets,
    models,
    requests,
    failures,
    tokens,
    partialTokens:
      !!history.modelsTruncated ||
      tokens.some((row) => row.reported < requests),
  };
}
