use std::time::Instant;

use dotonce::index::Index;
use dotonce::model::{SessionDoc, TurnIn};
use dotonce::retrieve::{Options, retrieve};

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) as usize
    }
}

fn vocab() -> Vec<String> {
    let syll = [
        "ka", "lo", "mi", "ne", "ru", "sa", "to", "vi", "ze", "po", "qu", "fe", "gi", "ho", "ju",
        "be",
    ];
    let mut v = Vec::new();
    for a in syll {
        for b in syll {
            for c in ["n", "r", "s", "t"] {
                v.push(format!("{a}{b}{c}"));
            }
        }
    }
    v
}

#[test]
#[ignore = "run with: cargo test --release --test speed -- --ignored --nocapture"]
fn retrieve_p95_under_50ms_at_100k_turns() {
    let words = vocab();
    let mut rng = Lcg(42);
    let mut ix = Index::open(":memory:").unwrap();
    let docs: Vec<SessionDoc> = (0..2_000)
        .map(|s| SessionDoc {
            id: format!("s{s}"),
            user_id: Some("u".to_string()),
            timestamp: Some(format!(
                "2023-{:02}-{:02}T10:00:00+00:00",
                s % 12 + 1,
                s % 28 + 1
            )),
            turns: (0..50)
                .map(|_| TurnIn {
                    speaker: "user".to_string(),
                    text: (0..30)
                        .map(|_| words[rng.next() % words.len()].as_str())
                        .collect::<Vec<_>>()
                        .join(" "),
                })
                .collect(),
        })
        .collect();
    let t0 = Instant::now();
    ix.add_sessions(&docs).unwrap();
    println!("ingest 100k turns: {:?}", t0.elapsed());

    let opt = Options {
        budget_tokens: 1_500,
        ..Options::default()
    };
    let mut times: Vec<f64> = (0..200)
        .map(|_| {
            let q = (0..4)
                .map(|_| words[rng.next() % words.len()].as_str())
                .collect::<Vec<_>>()
                .join(" ");
            let t = Instant::now();
            retrieve(&ix, &q, "u", None, &opt).unwrap();
            t.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    times.sort_by(f64::total_cmp);
    let p50 = times[100];
    let p95 = times[190];
    println!("retrieve p50 {p50:.1} ms, p95 {p95:.1} ms");
    assert!(p95 < 50.0, "p95 {p95:.1} ms");
}
