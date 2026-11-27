use chrono::{DateTime, Duration, FixedOffset, NaiveDate, NaiveTime, Utc};
use std::sync::{Arc, Mutex};

pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

impl<C: Clock + ?Sized> Clock for Arc<C> {
    fn now(&self) -> DateTime<Utc> {
        (**self).now()
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

#[derive(Clone, Debug)]
pub struct FakeClock {
    now: Arc<Mutex<DateTime<Utc>>>,
}

impl FakeClock {
    pub fn new(initial: DateTime<Utc>) -> Self {
        Self {
            now: Arc::new(Mutex::new(initial)),
        }
    }

    pub fn set(&self, t: DateTime<Utc>) {
        *self.now.lock().expect("FakeClock mutex poisoned") = t;
    }

    pub fn advance(&self, delta: Duration) {
        let mut guard = self.now.lock().expect("FakeClock mutex poisoned");
        *guard += delta;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> DateTime<Utc> {
        *self.now.lock().expect("FakeClock mutex poisoned")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarketHoursStatus {
    PreMarket,
    Open,
    Closed,
}

pub fn ist_offset() -> FixedOffset {
    FixedOffset::east_opt(5 * 3600 + 30 * 60).expect("valid IST +05:30 offset")
}

pub fn to_ist(utc: DateTime<Utc>) -> DateTime<FixedOffset> {
    utc.with_timezone(&ist_offset())
}

pub fn ist_date(utc: DateTime<Utc>) -> NaiveDate {
    to_ist(utc).date_naive()
}

pub fn market_hours_status(utc: DateTime<Utc>) -> MarketHoursStatus {
    let t = to_ist(utc).time();
    let open = NaiveTime::from_hms_opt(9, 15, 0).expect("valid market open time");
    let close = NaiveTime::from_hms_opt(15, 30, 0).expect("valid market close time");
    if t < open {
        MarketHoursStatus::PreMarket
    } else if t < close {
        MarketHoursStatus::Open
    } else {
        MarketHoursStatus::Closed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn ist_offset_and_to_ist_convert_utc_to_plus_0530_with_date_rollover() {
        assert_eq!(ist_offset().local_minus_utc(), 19_800);

        let open_utc = Utc.with_ymd_and_hms(2026, 11, 27, 3, 45, 0).unwrap();
        let open_ist = to_ist(open_utc);
        assert_eq!(
            open_ist.format("%Y-%m-%dT%H:%M:%S%:z").to_string(),
            "2026-11-27T09:15:00+05:30"
        );
        assert_eq!(ist_date(open_utc), NaiveDate::from_ymd_opt(2026, 11, 27).unwrap());

        let rollover_utc = Utc.with_ymd_and_hms(2026, 11, 27, 18, 30, 0).unwrap();
        let rollover_ist = to_ist(rollover_utc);
        assert_eq!(
            rollover_ist.format("%Y-%m-%dT%H:%M:%S%:z").to_string(),
            "2026-11-28T00:00:00+05:30"
        );
        assert_eq!(
            ist_date(rollover_utc),
            NaiveDate::from_ymd_opt(2026, 11, 28).unwrap()
        );
    }

    #[test]
    fn market_hours_status_boundaries_ist() {
        let before_open = Utc.with_ymd_and_hms(2026, 11, 27, 3, 44, 59).unwrap(); // 09:14:59 IST
        let at_open = Utc.with_ymd_and_hms(2026, 11, 27, 3, 45, 0).unwrap(); // 09:15:00 IST
        let before_close = Utc.with_ymd_and_hms(2026, 11, 27, 9, 59, 59).unwrap(); // 15:29:59 IST
        let at_close = Utc.with_ymd_and_hms(2026, 11, 27, 10, 0, 0).unwrap(); // 15:30:00 IST

        assert_eq!(market_hours_status(before_open), MarketHoursStatus::PreMarket);
        assert_eq!(market_hours_status(at_open), MarketHoursStatus::Open);
        assert_eq!(market_hours_status(before_close), MarketHoursStatus::Open);
        assert_eq!(market_hours_status(at_close), MarketHoursStatus::Closed);
    }

    #[test]
    fn fake_clock_supports_new_set_advance_and_shared_clones() {
        let t0 = Utc.with_ymd_and_hms(2026, 11, 27, 3, 45, 0).unwrap();
        let clock = FakeClock::new(t0);
        let cloned = clock.clone();
        let arc_clock: Arc<dyn Clock> = Arc::new(clock.clone());

        assert_eq!(clock.now(), t0);
        assert_eq!(cloned.now(), t0);
        assert_eq!(arc_clock.now(), t0);

        clock.advance(Duration::seconds(90));
        let t1 = t0 + Duration::seconds(90);
        assert_eq!(clock.now(), t1);
        assert_eq!(cloned.now(), t1);
        assert_eq!(arc_clock.now(), t1);

        let t2 = Utc.with_ymd_and_hms(2026, 11, 27, 10, 0, 0).unwrap();
        cloned.set(t2);
        assert_eq!(clock.now(), t2);
        assert_eq!(arc_clock.now(), t2);
    }

    #[test]
    fn system_clock_returns_current_utc() {
        let before = Utc::now();
        let now = SystemClock.now();
        let after = Utc::now();

        assert!(now >= before);
        assert!(now <= after);
    }
}
