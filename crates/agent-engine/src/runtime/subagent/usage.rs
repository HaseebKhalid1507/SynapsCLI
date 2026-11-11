//! What a worker run used: requests, tokens and an estimated cost.
//!
//! The orchestrating model picks cheap or expensive workers; this is how it
//! sees what one cost. Reported by `subagent_status`, `subagent_collect` and
//! the completion event.

use crate::SessionEvent;

/// What one worker run used, summed over every provider response that
/// reported usage. Field names follow the api-log telemetry.
///
/// Covers THIS run only: a worker continued with `subagent_resume` starts
/// from zero, not from the runs it was resumed from.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct WorkerUsage {
    /// Provider responses that reported usage (one per
    /// `SessionEvent::Usage`). The worker's own keep-warm pings count: they
    /// are billed, and are part of what the worker cost.
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// TTL split of `cache_write_tokens`; absent unless a response reported one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_5m_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_1h_tokens: Option<u64>,
    /// The latest request's prompt (input + cache read + cache write): how
    /// big the worker's context is now.
    pub context_tokens: u64,
    /// Estimated USD at API list prices ([`agent_core::pricing`]), rounded to
    /// 4 decimals. Absent when nothing was used or a response's model has no
    /// list price: unknown, never 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// Unrounded sum behind `cost_usd`.
    #[serde(skip)]
    cost: f64,
    /// A response used tokens on a model without a list price.
    #[serde(skip)]
    unpriced: bool,
}

impl WorkerUsage {
    /// Book one `SessionEvent::Usage` (other events are ignored). Priced per
    /// response, as billed (Haiku 5.5's tier depends on each prompt), with
    /// the event's model or else the worker's.
    pub fn add(&mut self, event: &SessionEvent, worker_model: &str) {
        let SessionEvent::Usage {
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_creation_input_tokens,
            cache_creation_5m,
            cache_creation_1h,
            model,
        } = event
        else {
            return;
        };
        let (input, output, read, write) = (
            *input_tokens,
            *output_tokens,
            *cache_read_input_tokens,
            *cache_creation_input_tokens,
        );
        self.requests += 1;
        self.input_tokens += input;
        self.output_tokens += output;
        self.cache_read_tokens += read;
        self.cache_write_tokens += write;
        crate::core::rpc_dispatch::merge_split(&mut self.cache_write_5m_tokens, *cache_creation_5m);
        crate::core::rpc_dispatch::merge_split(&mut self.cache_write_1h_tokens, *cache_creation_1h);
        self.context_tokens = input + read + write;

        let model = model.as_deref().unwrap_or(worker_model);
        if agent_core::pricing::has_list_price(model) {
            self.cost += agent_core::pricing::calculate_cost_optional_split(
                model,
                input,
                output,
                read,
                write,
                *cache_creation_5m,
                *cache_creation_1h,
            );
        } else if input + output + read + write > 0 {
            self.unpriced = true;
        }
        let used = self.input_tokens
            + self.output_tokens
            + self.cache_read_tokens
            + self.cache_write_tokens
            > 0;
        self.cost_usd = (used && !self.unpriced).then(|| (self.cost * 10_000.0).round() / 10_000.0);
    }

    /// One sentence for the completion event, `None` when no request was
    /// made: "Used 3 requests, 1.2k input, 61.2M cache-read and 370k output
    /// tokens, about $1.25 at list prices." Zero token kinds are left out,
    /// and so is the price clause without a `cost_usd`.
    pub fn sentence(&self) -> Option<String> {
        if self.requests == 0 {
            return None;
        }
        let plural = if self.requests == 1 { "" } else { "s" };
        let mut out = format!("Used {} request{plural}", self.requests);
        let kinds: Vec<String> = [
            (self.input_tokens, "input"),
            (self.cache_read_tokens, "cache-read"),
            (self.cache_write_tokens, "cache-write"),
            (self.output_tokens, "output"),
        ]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, kind)| format!("{} {kind}", fmt_tokens(n)))
        .collect();
        if let Some((last, rest)) = kinds.split_last() {
            out.push_str(", ");
            if !rest.is_empty() {
                out.push_str(&rest.join(", "));
                out.push_str(" and ");
            }
            out.push_str(last);
            out.push_str(" tokens");
        }
        if let Some(cost) = self.cost_usd {
            if cost > 0.0 && cost < 0.01 {
                out.push_str(", under $0.01 at list prices");
            } else {
                out.push_str(&format!(", about ${cost:.2} at list prices"));
            }
        }
        out.push('.');
        Some(out)
    }
}

/// `850`, `12.3k`, `370k`, `61.2M`, `1.25M`: below 1k exact, then one
/// decimal in k or two in M, trailing zeros dropped.
fn fmt_tokens(n: u64) -> String {
    fn trim(s: String) -> String {
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            s
        }
    }
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        let k = trim(format!("{:.1}", n as f64 / 1_000.0));
        // 999_950+ rounds up to "1000k": say 1M.
        if k == "1000" {
            "1M".into()
        } else {
            format!("{k}k")
        }
    } else {
        format!("{}M", trim(format!("{:.2}", n as f64 / 1_000_000.0)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(
        model: Option<&str>,
        i: u64,
        o: u64,
        r: u64,
        w: u64,
        w5: Option<u64>,
        w1: Option<u64>,
    ) -> SessionEvent {
        SessionEvent::Usage {
            input_tokens: i,
            output_tokens: o,
            cache_read_input_tokens: r,
            cache_creation_input_tokens: w,
            cache_creation_5m: w5,
            cache_creation_1h: w1,
            model: model.map(str::to_string),
        }
    }

    #[test]
    fn accumulates_requests_tokens_split_and_latest_context() {
        let mut u = WorkerUsage::default();
        u.add(
            &usage(None, 1_000, 200, 0, 50_000, None, None),
            "claude-sonnet-4-6",
        );
        u.add(&SessionEvent::Done, "claude-sonnet-4-6");
        u.add(
            &usage(None, 10, 300, 50_000, 2_000, Some(1_500), Some(500)),
            "claude-sonnet-4-6",
        );
        u.add(
            &usage(None, 20, 400, 52_000, 1_000, Some(1_000), None),
            "claude-sonnet-4-6",
        );
        assert_eq!(u.requests, 3, "only Usage events count");
        assert_eq!(
            (
                u.input_tokens,
                u.output_tokens,
                u.cache_read_tokens,
                u.cache_write_tokens
            ),
            (1_030, 900, 102_000, 53_000)
        );
        assert_eq!(u.cache_write_5m_tokens, Some(2_500));
        assert_eq!(u.cache_write_1h_tokens, Some(500));
        assert_eq!(
            u.context_tokens,
            20 + 52_000 + 1_000,
            "the latest prompt, not a sum"
        );

        let price = |i, o, r, w, w5, w1| {
            agent_core::pricing::calculate_cost_optional_split(
                "claude-sonnet-4-6",
                i,
                o,
                r,
                w,
                w5,
                w1,
            )
        };
        let expected = price(1_000, 200, 0, 50_000, None, None)
            + price(10, 300, 50_000, 2_000, Some(1_500), Some(500))
            + price(20, 400, 52_000, 1_000, Some(1_000), None);
        assert_eq!(u.cost_usd, Some((expected * 10_000.0).round() / 10_000.0));
        assert_eq!(u.cost_usd, Some(0.2471));

        let v = serde_json::to_value(&u).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "requests": 3, "input_tokens": 1030, "output_tokens": 900,
                "cache_read_tokens": 102000, "cache_write_tokens": 53000,
                "cache_write_5m_tokens": 2500, "cache_write_1h_tokens": 500,
                "context_tokens": 53020, "cost_usd": 0.2471
            })
        );
    }

    #[test]
    fn no_split_reported_leaves_the_split_fields_out() {
        let mut u = WorkerUsage::default();
        u.add(&usage(None, 10, 5, 0, 0, None, None), "claude-sonnet-4-6");
        let v = serde_json::to_value(&u).unwrap();
        assert!(
            v.get("cache_write_5m_tokens").is_none() && v.get("cache_write_1h_tokens").is_none(),
            "{v}"
        );
    }

    #[test]
    fn cost_is_absent_for_an_unknown_model_and_for_zero_usage() {
        let mut u = WorkerUsage::default();
        u.add(&usage(None, 1_000, 100, 0, 0, None, None), "openai/gpt-5");
        assert_eq!(u.requests, 1);
        assert_eq!(
            u.cost_usd, None,
            "no list price: unknown, not the Sonnet guess"
        );
        assert!(serde_json::to_value(&u).unwrap().get("cost_usd").is_none());

        // One unpriced response makes the whole run's cost unknown.
        let mut u = WorkerUsage::default();
        u.add(
            &usage(None, 1_000, 100, 0, 0, None, None),
            "claude-opus-5-5",
        );
        assert!(u.cost_usd.is_some());
        u.add(
            &usage(Some("z-ai/glm-5.2"), 1_000, 100, 0, 0, None, None),
            "claude-opus-5-5",
        );
        assert_eq!(u.cost_usd, None);

        let mut u = WorkerUsage::default();
        assert_eq!(u.cost_usd, None);
        u.add(&usage(None, 0, 0, 0, 0, None, None), "claude-opus-5-5");
        assert_eq!(
            (u.requests, u.cost_usd),
            (1, None),
            "nothing used: no cost, not $0"
        );

        // The event's own model wins over the worker's.
        let mut u = WorkerUsage::default();
        u.add(
            &usage(Some("claude-opus-5-5"), 1_000_000, 0, 0, 0, None, None),
            "openai/gpt-5",
        );
        assert_eq!(u.cost_usd, Some(4.0));
    }

    #[test]
    fn sentence_formats() {
        let mut u = WorkerUsage::default();
        assert_eq!(u.sentence(), None, "no requests, no sentence");
        u.add(
            &usage(None, 1_234, 370_000, 61_200_000, 0, None, None),
            "claude-opus-5-5",
        );
        for _ in 0..324 {
            u.add(&usage(None, 0, 0, 0, 0, None, None), "claude-opus-5-5");
        }
        assert_eq!(
            u.sentence().unwrap(),
            "Used 325 requests, 1.2k input, 61.2M cache-read and 370k output tokens, about $19.64 at list prices."
        );

        let mut u = WorkerUsage::default();
        u.add(&usage(None, 10, 5, 0, 0, None, None), "claude-sonnet-4-6");
        assert_eq!(
            u.sentence().unwrap(),
            "Used 1 request, 10 input and 5 output tokens, under $0.01 at list prices."
        );

        let mut u = WorkerUsage::default();
        u.add(&usage(None, 0, 2_500_000, 0, 0, None, None), "openai/gpt-5");
        assert_eq!(u.sentence().unwrap(), "Used 1 request, 2.5M output tokens.");

        let mut u = WorkerUsage::default();
        u.add(&usage(None, 0, 0, 0, 0, None, None), "openai/gpt-5");
        assert_eq!(u.sentence().unwrap(), "Used 1 request.");
    }

    #[test]
    fn token_counts_format_deterministically() {
        let cases = [
            (0, "0"),
            (999, "999"),
            (1_000, "1k"),
            (12_345, "12.3k"),
            (370_000, "370k"),
            (999_999, "1M"),
            (1_000_000, "1M"),
            (1_250_000, "1.25M"),
            (61_234_567, "61.23M"),
        ];
        for (n, want) in cases {
            assert_eq!(fmt_tokens(n), want, "{n}");
        }
    }
}
