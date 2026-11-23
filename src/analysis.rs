use tokio::{sync::mpsc, time::{self, Duration, Interval, MissedTickBehavior}};
use crate::types::MarketSnapshot;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger { MarketEvent, FiveMinute }
pub struct AnalysisTick { pub trigger: Trigger, pub snapshot: MarketSnapshot }

/// Coalesces event-driven updates and emits a five-minute context tick from the latest snapshot.
/// Callers should run risk freshness checks immediately before accepting any resulting signal.
pub struct HybridScheduler { events: mpsc::Receiver<MarketSnapshot>, timer: Interval, latest: Option<MarketSnapshot> }
impl HybridScheduler {
    pub fn new(events: mpsc::Receiver<MarketSnapshot>) -> Self {
        let mut timer = time::interval(Duration::from_secs(300));
        timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
        Self { events, timer, latest: None }
    }
    pub async fn next(&mut self) -> Option<AnalysisTick> {
        tokio::select! {
            event = self.events.recv() => match event {
                Some(snapshot) => { self.latest = Some(snapshot.clone()); Some(AnalysisTick { trigger: Trigger::MarketEvent, snapshot }) },
                None => None,
            },
            _ = self.timer.tick() => self.latest.clone().map(|snapshot| AnalysisTick { trigger: Trigger::FiveMinute, snapshot }),
        }
    }
}
