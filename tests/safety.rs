use chrono::{Duration, Utc};
use nifty_paper_agent::{agents::require_complete_cycle, config::Settings, data::ensure_fresh, journal::Journal, paper_broker::PaperBroker, types::{AgentOpinion, MarketSnapshot, Quote, Side, Signal}};
use uuid::Uuid;

fn setup() -> (Settings, MarketSnapshot, Signal) {
    let settings = Settings::load("settings.toml").unwrap();
    let now = Utc::now();
    let q = Quote { symbol:"NIFTY".into(), bid:100.0, ask:100.1, last:100.05, timestamp:now };
    let snap = MarketSnapshot { id:Uuid::new_v4(), created_at:now, quotes:vec![q], context:String::new() };
    let signal = Signal { id:"s1".into(), source:"test".into(), symbol:"NIFTY".into(), side:Side::Buy, quantity:1, limit_price:100.1, confidence_bps:7000, rationale:"test".into(), snapshot_id:Some(snap.id) };
    (settings,snap,signal)
}

#[test]
fn rejects_stale_quote() {
    let (_,_,mut s)=setup(); s.quotes[0].timestamp=Utc::now()-Duration::seconds(90);
    assert!(ensure_fresh(&s.quotes[0],15).is_err());
}
#[test]
fn duplicate_signal_is_only_journaled_once() {
    let (_,_,s)=setup(); let db=tempfile::NamedTempFile::new().unwrap(); let j=Journal::open(db.path().to_str().unwrap()).unwrap();
    assert!(j.record_signal(&s).unwrap()); assert!(!j.record_signal(&s).unwrap());
}
#[test]
fn capital_limit_blocks_trade() {
    let (settings,snap,mut s)=setup(); s.quantity=50; s.limit_price=1000.0;
    let mut broker=PaperBroker::new(settings.paper);
    assert!(broker.execute(&s,&snap.quotes[0],&snap,0,0,15).is_err());
}
#[test]
fn incomplete_worker_cycle_is_not_accepted() {
    let opinions:Vec<AgentOpinion>=(0..9).map(|_| AgentOpinion { objective:"same".into(), stance:"neutral".into(), confidence_bps:0, evidence:String::new(), signal:None }).collect();
    assert!(require_complete_cycle(&opinions,10).is_err());
}
