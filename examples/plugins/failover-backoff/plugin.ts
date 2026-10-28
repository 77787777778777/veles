// failover-backoff: an override of veles' `failover.decide` point.
//
// veles asks `failover.decide` every time a call to a model route fails.
// Its own answer is `{action: "policy"}`: apply the configured policy
// (`[[failover]]` rules, `/failover`, by default stop). This plugin
// answers instead for the two cases it cares about and hands everything
// else to `next`, so the configured policy still decides the rest.
//
//   rate limited (429): wait 30 s and retry, then 60 s and retry, then go
//                       to the next route in the chain (or stop at its end)
//   server error (5xx): try the next route at once
//   anything else:      whatever the configured policy says
//
// Nothing here touches veles' source: an update leaves this file alone,
// and `veles plugins unload failover-backoff` restores the default.

interface Route {
  provider: string;
  model: string;
}

interface DecideInput {
  attempt: number;
  hops: number;
  route: Route;
  error: { kind: string; status: number | null; message: string };
  remaining_routes: Route[];
}

const WAITS = [30, 60];

await veles.registerService(
  {
    name: "failover.decide",
    description: "on a rate limit, wait and retry twice, then move on",
  },
  async (input: DecideInput, cx: { next: (p: unknown) => Promise<unknown> }) => {
    if (input.error.kind === "rate_limited") {
      if (input.attempt < WAITS.length) {
        return {
          action: "wait",
          seconds: WAITS[input.attempt],
          reason: `${input.route.provider} is rate limited; waiting before retry ${input.attempt + 1}`,
        };
      }
      const next = input.remaining_routes[0];
      if (next !== undefined) {
        return {
          action: "goto",
          provider: next.provider,
          model: next.model,
          reason: `${input.route.provider} is still rate limited after ${WAITS.length} retries`,
        };
      }
      return { action: "stop", reason: "rate limited, and no route is left to try" };
    }
    if (input.error.kind === "server") {
      return { action: "advance", reason: `${input.route.provider} answered a server error` };
    }
    return cx.next(input);
  },
);
