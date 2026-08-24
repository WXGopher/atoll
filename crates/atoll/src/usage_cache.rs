//! The polled view of both agents' rate-limit windows, and the numbers every
//! readout in Atoll is built from.
//!
//! Claude Code's numbers reach Atoll through the status line bridge's cache and
//! Codex's through its newest rollout file. Both are files another process
//! writes, so this is a poll rather than a subscription: cheap at this interval,
//! and free when nothing is happening.
//!
//! Everything that turns a reading into a number is a pure function of the
//! reading, so the whole of it is testable from literal values.

use atoll_core::protocol::HookSource;
use atoll_core::usage::{self, ClaudeLimits, CodexUsage, WindowUsage};
use serde_json::Value;

use crate::util::home_dir;

/// How long a reading is reused before the caches are read again.
pub const REFRESH_SECS: u64 = 30;

/// How much of a rate-limit window has to be left before it stops being worth
/// worrying about, and before it becomes worth stopping for.
///
/// The same two numbers the user's terminal status line uses, so a percentage
/// does not change colour depending on where they read it.
pub const LEFT_COMFORTABLE: i64 = 50;
pub const LEFT_TIGHT: i64 = 20;

/// Which of the three colours in `ui/common.slint` a remaining percentage takes.
pub fn left_tier(left: i64) -> &'static str {
    if left >= LEFT_COMFORTABLE {
        "good"
    } else if left >= LEFT_TIGHT {
        "warn"
    } else {
        "low"
    }
}

/// One rate-limit window in the shape everything downstream reads it: a name,
/// how much is **left**, and when it rolls over.
///
/// Both agents report consumption in their own spelling — Claude's `percent`,
/// Codex's `used_percent` — and both are turned around here, once, so that no
/// caller has to remember which direction it is holding. See [`remaining`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateWindow {
    /// `Session`, `Week`, `5h`, or a scoped model's name.
    pub label: String,
    /// 0–100, how much of the window is left.
    pub left: i64,
    pub resets_at: Option<u64>,
}

/// The last usage reading, reused for [`REFRESH_SECS`].
#[derive(Debug, Default, Clone)]
pub struct UsageSnapshot {
    /// Claude's windows, from the OAuth usage endpoint.
    pub claude: ClaudeLimits,
    pub codex: Option<CodexUsage>,
    /// `None` until the first read.
    pub refreshed_at: Option<u64>,
}

impl UsageSnapshot {
    /// Re-read the cheap, local half: Codex's rollout files. Claude's numbers
    /// come off the network and arrive separately; see [`fetch_claude_limits`].
    pub fn read(now: u64) -> Self {
        Self {
            claude: ClaudeLimits::default(),
            codex: home_dir().and_then(|home| usage::scan_codex_usage(&home).ok().flatten()),
            refreshed_at: Some(now),
        }
    }

    /// Re-read the caches if the reading has gone stale, then hand back a copy.
    pub fn refreshed(&mut self, now: u64) -> UsageSnapshot {
        let due = match self.refreshed_at {
            Some(at) => now.saturating_sub(at) >= REFRESH_SECS,
            None => true,
        };
        if due {
            let carried = self.claude.clone();
            *self = Self::read(now);
            // Claude's half is fetched on its own schedule, off this thread.
            self.claude = carried;
        }
        self.clone()
    }

    /// Codex's two windows, short first, each absent if unread.
    fn codex_windows(&self) -> [Option<WindowUsage>; 2] {
        match &self.codex {
            Some(codex) => [codex.primary, codex.secondary],
            None => [None, None],
        }
    }

    /// What to call one of Codex's windows.
    ///
    /// Codex reports its own window lengths, and they are not exactly five hours
    /// and seven days — see [`window_label`]. Anything a week or longer is just
    /// "Week", which is what people call it.
    fn codex_label(index: usize, window: &WindowUsage) -> String {
        match window.window_minutes {
            Some(minutes) if minutes >= 1_440 => "Week".to_string(),
            Some(_) => window_label(window.window_minutes),
            None => ["5h", "Week"][index.min(1)].to_string(),
        }
    }

    /// Every rate-limit window this agent reported, oldest spelling turned
    /// around into "how much is left".
    ///
    /// Either of Codex's two windows may be missing on its own, and an agent
    /// nobody has run yields nothing at all rather than zeroes it never
    /// measured.
    pub fn windows(&self, agent: HookSource) -> Vec<RateWindow> {
        match agent {
            // Every window the endpoint reported, including one per scoped
            // model — those are the ones a heavy user actually runs out of.
            HookSource::Claude => self
                .claude
                .limits
                .iter()
                .map(|limit| RateWindow {
                    label: limit.label.clone(),
                    left: remaining(limit.percent),
                    resets_at: limit.resets_at,
                })
                .collect(),
            HookSource::Codex => self
                .codex_windows()
                .into_iter()
                .enumerate()
                .filter_map(|(index, window)| {
                    let window = window?;
                    Some(RateWindow {
                        label: Self::codex_label(index, &window),
                        left: remaining(window.used_percent),
                        resets_at: window.resets_at,
                    })
                })
                .collect(),
        }
    }

    /// The window that will stop the user first: the one with the least left.
    ///
    /// This is the number the taskbar readout shows, because a chip has room for
    /// exactly one window and the binding one is the only honest choice. Ties go to the window reported first, which is the shorter
    /// one — running out of five hours bites before running out of a week.
    ///
    /// `None` for an agent with no reading at all.
    pub fn tightest_window(&self, agent: HookSource) -> Option<RateWindow> {
        self.windows(agent)
            .into_iter()
            .min_by_key(|window| window.left)
    }

    /// The windows on one line and without their reset times, for places
    /// with a line to spare rather than a panel: `5h 24% · 7d 41%`.
    pub fn usage_summary(&self, agent: HookSource) -> String {
        self.windows(agent)
            .iter()
            .map(|window| format!("{} {}%", window.label, window.left))
            .collect::<Vec<_>>()
            .join(" · ")
    }

    /// The one-line form the tray tooltip shows, leading with Claude and falling
    /// back to Codex.
    pub fn compact(&self) -> String {
        let claude = self.usage_summary(HookSource::Claude);
        if claude.is_empty() {
            self.usage_summary(HookSource::Codex)
        } else {
            claude
        }
    }

    /// One line per agent for the headless log. Agents with no reading are left
    /// out entirely rather than shown as zero.
    pub fn detail_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for agent in [HookSource::Claude, HookSource::Codex] {
            let line = self.usage_summary(agent);
            if !line.is_empty() {
                lines.push(format!("{} {}", agent.as_str(), line.replace(" · ", " ")));
            }
        }
        lines
    }
}

/// How much of a window is **left**, from how much of it is gone.
///
/// Both agents report consumption — Claude's `/api/oauth/usage` calls it
/// `percent`, Codex's rollout calls it `used_percent` — and Atoll shows the
/// other end, because "12% left" is the number that makes someone slow down and
/// "88% used" is the number they have to subtract first.
///
/// Clamped, because a window can report past its own limit.
pub fn remaining(used_percent: f64) -> i64 {
    (100.0 - used_percent).round().clamp(0.0, 100.0) as i64
}

/// `HH:MM` in local time, for a Unix timestamp and a UTC offset in seconds.
pub fn local_clock(unix: u64, offset_secs: i64) -> String {
    let local = (unix as i64 + offset_secs).rem_euclid(86_400);
    format!("{:02}:{:02}", local / 3_600, (local % 3_600) / 60)
}

/// How to say when a window rolls over, or `None` for one that already has.
///
/// A clock time is unambiguous within the day, and the seven-day window is the
/// only one that ever reaches past it — so anything further out gets a weekday
/// in front. A full date would cost more room in the panel than it buys.
pub fn reset_label(resets_at: Option<u64>, now: u64, offset_secs: i64) -> Option<String> {
    let at = resets_at?;
    if at <= now {
        return None;
    }
    let clock = local_clock(at, offset_secs);
    if at - now < 86_400 {
        return Some(clock);
    }
    Some(format!("{} {clock}", weekday(at, offset_secs)))
}

/// The three-letter local weekday of a Unix timestamp.
///
/// 1970-01-01 was a Thursday, which is where the `+ 4` comes from.
fn weekday(unix: u64, offset_secs: i64) -> &'static str {
    const NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let days = (unix as i64 + offset_secs).div_euclid(86_400);
    NAMES[(days + 4).rem_euclid(7) as usize]
}

/// Codex reports its own window length, and it is not always exactly 5 h / 7 d —
/// real rollout files carry 299, 300, 10 079, and 10 080 minutes. Round to the
/// nearest familiar name rather than claiming a precision we do not have.
pub fn window_label(window_minutes: Option<u64>) -> String {
    match window_minutes {
        Some(minutes) if minutes >= 1_440 => format!("{}d", (minutes as f64 / 1_440.0).round()),
        Some(minutes) if minutes >= 60 => format!("{}h", (minutes as f64 / 60.0).round()),
        Some(minutes) => format!("{minutes}m"),
        None => "usage".to_string(),
    }
}

// -------------------------------------------------------- the network fetch

/// Claude's rate-limit windows, from the endpoint if it answers and from
/// whatever is on disk if it does not.
///
/// **Blocking, and not for the UI thread.** The caller runs this on a worker and
/// takes the result through a channel.
///
/// The chain is deliberate: a fresh reading, else Atoll's own cache however
/// stale, else the cache the user's own status line script keeps. Numbers a
/// minute old beat no numbers, and numbers an hour old beat a blank panel — the
/// panel says when each window resets, so a stale reading looks stale.
pub fn fetch_claude_limits(now: u64) -> ClaudeLimits {
    let own_cache = usage::claude_usage_cache_path().ok();
    let cached = own_cache
        .as_deref()
        .and_then(|path| usage::read_claude_usage_cache(path).ok().flatten())
        .unwrap_or_default();

    if !cached.is_stale(now, usage::CLAUDE_USAGE_TTL_SECS) {
        return cached;
    }
    if let Some(fresh) = ask_the_endpoint(now, own_cache.as_deref()) {
        return fresh;
    }

    // Whatever we fall back to is stamped with *now*, failure included. Without
    // that, a reading with no timestamp reads as stale on the very next tick and
    // the fetch runs again half a second later — which is how a fallback becomes
    // a request loop against an endpoint that is already rate-limiting us.
    let mut fallback = if cached.is_empty() {
        usage::foreign_usage_cache_path()
            .ok()
            .and_then(|path| usage::read_claude_usage_cache(&path).ok().flatten())
            .unwrap_or_default()
    } else {
        cached
    };
    fallback.fetched_at = Some(now);
    fallback
}

/// One authenticated GET. `None` for any failure at all — no token, no network,
/// a rate-limit rebuff — because every one of them means "use what you have".
fn ask_the_endpoint(now: u64, cache_path: Option<&std::path::Path>) -> Option<ClaudeLimits> {
    let token = usage::claude_credentials_path()
        .ok()
        .and_then(|path| usage::read_claude_oauth_token(&path))?;

    // The token lives in this scope and nowhere else: not in a log line, not in
    // an error, not on disk.
    let authorization = format!("Bearer {token}");
    drop(token);

    let body = crate::app::net::get_json(
        "api.anthropic.com",
        "/api/oauth/usage",
        &[
            ("Authorization", authorization.as_str()),
            ("anthropic-beta", usage::CLAUDE_USAGE_BETA),
            ("Accept", "application/json"),
        ],
    )
    .ok()?;
    drop(authorization);

    let value: Value = serde_json::from_str(&body).ok()?;
    let mut limits = usage::parse_claude_limits(&value);
    if limits.is_empty() {
        return None;
    }
    limits.fetched_at = Some(now);
    if let Some(path) = cache_path {
        let _ = usage::write_claude_usage_cache(path, &value, now);
    }
    Some(limits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use atoll_core::usage::UsageLimit;

    const NOW: u64 = 1_787_000_000;

    /// One agent's windows as `name left%`, which is the shape every caller
    /// renders one way or another.
    fn labelled(snapshot: &UsageSnapshot, agent: HookSource) -> Vec<String> {
        snapshot
            .windows(agent)
            .iter()
            .map(|window| format!("{} left {}%", window.label, window.left))
            .collect()
    }

    fn window(used: f64, minutes: Option<u64>, resets_at: Option<u64>) -> WindowUsage {
        WindowUsage {
            used_percent: used,
            resets_at,
            window_minutes: minutes,
        }
    }

    fn limit(kind: &str, label: &str, percent: f64, resets_at: Option<u64>) -> UsageLimit {
        UsageLimit {
            kind: kind.to_string(),
            label: label.to_string(),
            percent,
            resets_at,
        }
    }

    /// The three windows the endpoint really returns, in the order it returns
    /// them: the session, the whole week, and the week for one model. The
    /// percentages are **consumption**, which is how both agents report.
    fn claude() -> ClaudeLimits {
        ClaudeLimits {
            limits: vec![
                limit("session", "Session", 8.0, Some(NOW + 7_200)),
                limit("weekly_all", "Week", 31.0, Some(NOW + 5 * 86_400)),
                limit("weekly_scoped", "Fable", 27.0, Some(NOW + 5 * 86_400)),
            ],
            fetched_at: Some(NOW),
        }
    }

    fn codex() -> CodexUsage {
        CodexUsage {
            primary: Some(window(7.4, Some(299), None)),
            secondary: Some(window(58.0, Some(10_080), None)),
            plan_type: Some("prolite".into()),
            source: None,
        }
    }

    #[test]
    fn every_claude_window_gets_its_own_line() {
        let snapshot = UsageSnapshot {
            claude: claude(),
            ..UsageSnapshot::default()
        };
        let windows = snapshot.windows(HookSource::Claude);

        assert_eq!(windows.len(), 3, "including the per-model weekly window");
        // 8 % used reads as 92 % left, and so on down. A scoped window is named
        // after its model — that is the only thing telling two of them apart.
        assert_eq!(
            labelled(&snapshot, HookSource::Claude),
            vec![
                "Session left 92%".to_string(),
                "Week left 69%".to_string(),
                "Fable left 73%".to_string(),
            ]
        );

        // Within the day the reset is a clock; days out it takes a weekday too.
        let offset = 8 * 3_600;
        assert_eq!(
            reset_label(windows[0].resets_at, NOW, offset)
                .unwrap()
                .split_whitespace()
                .count(),
            1
        );
        assert_eq!(
            reset_label(windows[1].resets_at, NOW, offset)
                .unwrap()
                .split_whitespace()
                .count(),
            2
        );
    }

    /// The thresholds the user's own terminal status line uses. A percentage
    /// must not change colour depending on where it is read.
    #[test]
    fn the_percentage_colour_turns_at_fifty_and_at_twenty() {
        assert_eq!(left_tier(100), "good");
        assert_eq!(left_tier(LEFT_COMFORTABLE), "good");
        assert_eq!(left_tier(LEFT_COMFORTABLE - 1), "warn");
        assert_eq!(left_tier(LEFT_TIGHT), "warn");
        assert_eq!(left_tier(LEFT_TIGHT - 1), "low");
        assert_eq!(left_tier(0), "low");
        // Nothing outside 0–100 reaches here, but nothing panics if it does.
        assert_eq!(left_tier(-5), "low");
        assert_eq!(left_tier(400), "good");
    }

    /// The one that must never invert: both agents report how much is **gone**,
    /// and Atoll shows how much is **left**. 8 % used is 92 % left.
    #[test]
    fn percentages_are_shown_as_what_is_left_not_what_is_spent() {
        assert_eq!(remaining(8.0), 92);
        assert_eq!(remaining(0.0), 100);
        assert_eq!(remaining(100.0), 0);
        assert_eq!(remaining(31.4), 69);
        // A window can report past its own limit; nobody has -7 % left.
        assert_eq!(remaining(107.0), 0);
        assert_eq!(remaining(-3.0), 100);
    }

    #[test]
    fn codex_keeps_its_own_window_lengths_and_the_same_direction() {
        let snapshot = UsageSnapshot {
            codex: Some(codex()),
            ..UsageSnapshot::default()
        };
        // 299 minutes is "5h"; a week or more is just "Week". And Codex's
        // `used_percent` is converted the same way Claude's `percent` is: 7.4 %
        // used is 93 % left, 58 % used is 42 % left.
        assert_eq!(
            labelled(&snapshot, HookSource::Codex),
            vec!["5h left 93%".to_string(), "Week left 42%".to_string()]
        );
    }

    /// The resting panel has one line per agent, so it shows the window that
    /// will stop the user first rather than the one that happens to be listed
    /// first.
    #[test]
    fn the_tightest_window_is_the_one_with_the_least_left() {
        let snapshot = UsageSnapshot {
            claude: claude(),
            codex: Some(codex()),
            ..UsageSnapshot::default()
        };

        // Session 92 %, Week 69 %, Fable 73 % — the week is the binding one, and
        // it is neither the first nor the last window reported.
        let claude = snapshot.tightest_window(HookSource::Claude).unwrap();
        assert_eq!(claude.label, "Week");
        assert_eq!(claude.left, 69);
        assert_eq!(claude.resets_at, Some(NOW + 5 * 86_400));

        // Codex: 5h 93 %, Week 42 %.
        let codex = snapshot.tightest_window(HookSource::Codex).unwrap();
        assert_eq!((codex.label.as_str(), codex.left), ("Week", 42));

        // A tie goes to the window reported first — the shorter one, which runs
        // out sooner in wall-clock terms.
        let tied = UsageSnapshot {
            claude: ClaudeLimits {
                limits: vec![
                    limit("session", "Session", 40.0, None),
                    limit("weekly_all", "Week", 40.0, None),
                ],
                fetched_at: Some(NOW),
            },
            ..UsageSnapshot::default()
        };
        assert_eq!(
            tied.tightest_window(HookSource::Claude).unwrap().label,
            "Session"
        );
    }

    /// Half a reading is still a reading: Codex reports either window on its
    /// own, and the agent that reported nothing has no tightest window at all
    /// rather than a zero one.
    #[test]
    fn a_missing_window_is_skipped_rather_than_counted_as_empty() {
        let one_window = UsageSnapshot {
            codex: Some(CodexUsage {
                primary: None,
                secondary: Some(window(58.0, Some(10_080), None)),
                plan_type: None,
                source: None,
            }),
            ..UsageSnapshot::default()
        };
        let tightest = one_window.tightest_window(HookSource::Codex).unwrap();
        assert_eq!((tightest.label.as_str(), tightest.left), ("Week", 42));
        assert_eq!(one_window.windows(HookSource::Codex).len(), 1);

        let nothing = UsageSnapshot::default();
        assert_eq!(nothing.tightest_window(HookSource::Claude), None);
        assert_eq!(nothing.tightest_window(HookSource::Codex), None);
    }

    #[test]
    fn an_agent_nobody_has_run_shows_nothing_rather_than_zero() {
        let nothing = UsageSnapshot::default();
        assert!(nothing.windows(HookSource::Claude).is_empty());
        assert!(nothing.windows(HookSource::Codex).is_empty());
        assert_eq!(nothing.usage_summary(HookSource::Claude), "");
        assert_eq!(nothing.compact(), "");
        assert!(nothing.detail_lines().is_empty());
    }

    #[test]
    fn the_tooltip_leads_with_claude_and_falls_back_to_codex() {
        let both = UsageSnapshot {
            claude: claude(),
            codex: Some(codex()),
            ..UsageSnapshot::default()
        };
        assert_eq!(both.compact(), "Session 92% · Week 69% · Fable 73%");

        let codex_only = UsageSnapshot {
            codex: Some(codex()),
            ..UsageSnapshot::default()
        };
        assert_eq!(codex_only.compact(), "5h 93% · Week 42%");
        assert_eq!(codex_only.detail_lines(), vec!["codex 5h 93% Week 42%"]);
    }

    #[test]
    fn a_reset_already_in_the_past_is_dropped_but_the_percentage_stays() {
        let stale = UsageSnapshot {
            claude: ClaudeLimits {
                limits: vec![limit("session", "Session", 23.0, Some(NOW - 60))],
                fetched_at: Some(NOW),
            },
            ..UsageSnapshot::default()
        };
        assert_eq!(
            labelled(&stale, HookSource::Claude),
            vec!["Session left 77%".to_string()]
        );
        // The percentage survives; the reset is what the renderer drops.
        let window = &stale.windows(HookSource::Claude)[0];
        assert_eq!(reset_label(window.resets_at, NOW, 0), None);
    }

    #[test]
    fn a_reset_within_the_day_is_a_clock_and_beyond_it_a_weekday() {
        // 1970-01-01 was a Thursday, which is what anchors the weekday maths.
        assert_eq!(weekday(0, 0), "Thu");
        assert_eq!(weekday(86_400, 0), "Fri");
        assert_eq!(weekday(6 * 86_400, 0), "Wed");

        assert_eq!(reset_label(Some(NOW + 3_600), NOW, 0).unwrap().len(), 5);
        let far = reset_label(Some(NOW + 3 * 86_400), NOW, 0).unwrap();
        assert_eq!(far.split(' ').count(), 2, "got {far}");
        assert_eq!(far.split(' ').next().unwrap().len(), 3);

        // Exactly a day out already needs the weekday, and the past gets none.
        assert!(
            reset_label(Some(NOW + 86_400), NOW, 0)
                .unwrap()
                .contains(' ')
        );
        assert_eq!(reset_label(Some(NOW - 1), NOW, 0), None);
        assert_eq!(reset_label(None, NOW, 0), None);
    }

    #[test]
    fn the_local_clock_wraps_the_day_in_both_directions() {
        // Midnight UTC — the constant is already a whole number of days.
        let midnight = 1_787_011_200u64;
        assert_eq!(local_clock(midnight, 0), "00:00");
        assert_eq!(local_clock(midnight, 8 * 3_600), "08:00");
        // Eight hours before midnight UTC is 16:00 the previous local day.
        assert_eq!(local_clock(midnight, -8 * 3_600), "16:00");
        assert_eq!(local_clock(midnight + 3_600 + 1_800, 0), "01:30");
    }

    #[test]
    fn codex_window_labels_round_to_familiar_names() {
        // Real rollout files carry all four of these.
        assert_eq!(window_label(Some(299)), "5h");
        assert_eq!(window_label(Some(300)), "5h");
        assert_eq!(window_label(Some(10_079)), "7d");
        assert_eq!(window_label(Some(10_080)), "7d");
        assert_eq!(window_label(Some(30)), "30m");
        assert_eq!(window_label(None), "usage");
    }
}
