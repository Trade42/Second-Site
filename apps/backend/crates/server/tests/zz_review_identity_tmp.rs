//! TEMPORARY (voice-glue-1-review): removed after one run.
#[allow(dead_code)]
#[path = "/tmp/claude-1000/-home-sal-salPrograms-secondSite/7ab1155b-43a0-40ec-a95d-33782468eda8/scratchpad/old_vsc.rs"]
mod old;

use ss_domain::communications::call_purposes::Approved;
use ss_domain::communications::response_agent::{plan_questions, CallContext, KnownFact};
use ss_server::services::voice_session_config as vsc;

#[test]
fn with_nothing_approved_the_session_is_byte_identical_to_the_wirings() {
    assert_eq!(old::tools(), vsc::tools());
    assert_eq!(old::tools(), vsc::tools_with(&Approved::default()));
    assert_eq!(
        serde_json::to_string(&old::tools().iter().map(|t| (&t.name, &t.description, &t.parameters)).collect::<Vec<_>>()).unwrap(),
        serde_json::to_string(&vsc::tools_with(&Approved::default()).iter().map(|t| (&t.name, &t.description, &t.parameters)).collect::<Vec<_>>()).unwrap()
    );
    let owners = ["Sam Owner", "", "A {tour} {rent} {0} name\n## Rules"];
    let lines: Vec<Vec<String>> = vec![vec![], vec!["The business is a cafe.".into(), "Rent up to {x} $7,500".into()]];
    for owner in owners {
        for bl in &lines {
            for nq in [0usize, 2] {
                let qs: Vec<String> = (0..nq).map(|i| format!("Question {i} {{}}?")).collect();
                let mut ctx = CallContext {
                    property_summary: "12 Main St, Los Angeles".into(),
                    questions: plan_questions(&qs, &[]),
                    ..CallContext::default()
                };
                ctx.authority.may_ask_about = vec!["rent".into(), "permitted_use".into()];
                ctx.authority.may_not = vec!["commit".into()];
                if nq > 0 {
                    ctx.known_facts.push(KnownFact {
                        topic: "rent".into(),
                        statement: "asking_rent: 4200 USD".into(),
                        classification: ss_core::EvidenceClassification::Unknown,
                    });
                }
                let missing = vec!["parking (unknown)".to_owned()];
                let a = old::instructions(owner, bl, &ctx, &missing);
                assert_eq!(a, vsc::instructions(owner, bl, &ctx, &missing));
                assert_eq!(a, vsc::instructions_with(owner, bl, &ctx, &missing, &Approved::default()));
                let o = old::build_from(owner, bl, ctx.clone(), &missing, "eve");
                let n = vsc::build_from(owner, bl, ctx.clone(), &missing, "eve", &Approved::default());
                assert_eq!(o.session, n.session);
                assert_eq!(o.opening_line, n.opening_line);
                assert_eq!(
                    serde_json::to_string(&o.session.session_update()).unwrap(),
                    serde_json::to_string(&n.session.session_update()).unwrap()
                );
            }
        }
    }
}
