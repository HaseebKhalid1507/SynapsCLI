//! Loading a session's agent-card sidecar (agent-ui v3.2 #12). Core decodes
//! it and checks its structural bounds ([`session_surfaces::load`]); then
//! every card's doc is revalidated here, where the doc rules live: a doc
//! this engine wouldn't have written makes the whole sidecar unreadable
//! (§3.11 step 3); only then does core's `recover` see it. Inert: nothing
//! calls it yet (C4a).

use agent_core::core::session_surfaces::{self, Loaded, Sidecar, UiFs};

/// `<session_id>.ui`, decoded, bounded and revalidated. An I/O error is
/// `Err` (the store fails closed); a file that fails any check is
/// [`Loaded::Unreadable`].
pub fn load<F: UiFs + ?Sized>(fs: &F, session_id: &str) -> std::io::Result<Loaded> {
    Ok(revalidated(session_surfaces::load(fs, session_id)?))
}

/// `loaded`, with an image whose docs don't all revalidate made unreadable.
pub fn revalidated(loaded: Loaded) -> Loaded {
    match loaded {
        Loaded::Image(s) => match check_docs(&s) {
            Ok(()) => Loaded::Image(s),
            Err(e) => Loaded::Unreadable(e),
        },
        other => other,
    }
}

fn check_docs(s: &Sidecar) -> Result<(), String> {
    for c in &s.cards {
        super::revalidate(&c.state.doc).map_err(|e| format!("card {}: {e}", c.state.id))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::core::session_surfaces::{
        encode_sidecar, sidecar_name, CardRecord, ModelFs, ROOT_BASE,
    };
    use agent_core::core::ui_types::{CardDoc, Origin, Phase, SurfaceState, Verb, WaitMode};
    use serde_json::json;

    const SID: &str = "20261123-120000-a1b2";
    const LINEAGE: &str = "0123456789abcdef0123456789abcdef";

    fn good_doc() -> CardDoc {
        let input = json!({"question": "Which approach?", "options": ["Refactor", "Rewrite"]});
        let Ok(super::super::UiInput::Create { doc, .. }) = super::super::parse_input(&input)
        else {
            panic!("a valid create");
        };
        doc
    }

    fn card(n: u8, doc: CardDoc) -> CardRecord {
        CardRecord {
            state: SurfaceState {
                id: format!("s_{n:016x}"),
                call_id: format!("toolu_{n}"),
                rev: 1,
                contract: format!("c1:{}", "0".repeat(32)),
                catalog: 1,
                mode: WaitMode::Live,
                phase: Phase::Open,
                offers: vec![Verb::Answer, Verb::Skip],
                doc,
                origin: Origin {
                    turn: None,
                    task: "Cut the 0.13 release".into(),
                    stopped: None,
                },
                epoch: 0,
                resume: false,
                actions: Vec::new(),
                raised_at: "2026-11-23T11:00:00+00:00".into(),
                downgraded: None,
                answer: None,
            },
            log: Vec::new(),
        }
    }

    fn stored(cards: Vec<CardRecord>) -> std::sync::Arc<ModelFs> {
        let mut s = Sidecar::new(SID, LINEAGE, ROOT_BASE);
        s.cards = cards;
        let fs = ModelFs::new();
        fs.put(
            &sidecar_name(SID),
            &encode_sidecar(&s).expect("within bounds"),
        );
        fs
    }

    /// Every doc the engine would write revalidates: the image loads.
    #[test]
    fn a_sidecar_of_valid_docs_loads() {
        let fs = stored(vec![card(1, good_doc()), card(2, good_doc())]);
        let Loaded::Image(s) = load(&*fs, SID).unwrap() else {
            panic!("an image")
        };
        assert_eq!(s.cards.len(), 2);
        assert_eq!(load(&*ModelFs::new(), SID).unwrap(), Loaded::Missing);
    }

    /// A doc that passes core's structural bounds but not the doc rules
    /// (one option; a question on two lines) makes the whole sidecar
    /// unreadable, naming the card: recovery never sees it as an image.
    #[test]
    fn a_doc_that_fails_revalidation_makes_the_sidecar_unreadable() {
        let bad: Vec<CardDoc> = vec![
            serde_json::from_value(json!({"question": "Which approach?", "options": [{"label": "Only"}]})).unwrap(),
            serde_json::from_value(json!({"question": "Which\napproach?", "options": [{"label": "A"}, {"label": "B"}]}))
                .unwrap(),
        ];
        for doc in bad {
            let fs = stored(vec![card(1, good_doc()), card(2, doc)]);
            match load(&*fs, SID).unwrap() {
                Loaded::Unreadable(e) => assert!(e.contains(&format!("s_{:016x}", 2)), "{e}"),
                other => panic!("{other:?}"),
            }
        }
    }
}
