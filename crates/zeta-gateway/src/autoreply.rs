//! Auto replies ("trả lời tự động"): scripts the account writes, each saying
//! which chats it answers, on what words, when, and with what text. The
//! gateway answers new incoming messages with the first script that fits.
//!
//! What keeps it polite and safe:
//! - only messages that just came in (not history, not the account's own,
//!   not Zalo's notices, not the "Cloud của tôi" thread);
//! - a script answers a chat at most once per its pause (at least a minute),
//!   and any chat gets at most [`MAX_PER_CHAT_PER_HOUR`] auto replies an hour,
//!   so two auto-answering accounts cannot talk to each other for long;
//! - a script can wait until the account has been quiet in that chat for a
//!   while ("only when I am away");
//! - everything sent is logged, and every script can be tried without sending.

use serde::{Deserialize, Serialize};

/// Most auto replies a chat gets in an hour, whatever the scripts say.
pub const MAX_PER_CHAT_PER_HOUR: usize = 6;

/// Messages older than this when they reach the gateway are not answered
/// (ms): a reconnection catching up must not answer old news.
pub const FRESH_MS: i64 = 3 * 60 * 1000;

/// Shortest pause between two answers of a script in one chat (minutes).
pub const MIN_COOLDOWN_MINUTES: u32 = 1;

/// Most scripts kept.
pub const MAX_RULES: usize = 50;

/// Local time for schedules and `{gio}`: `ZETACHAT_UTC_OFFSET_MINUTES`, else
/// Vietnam (UTC+7).
pub fn utc_offset_minutes() -> i32 {
    std::env::var("ZETACHAT_UTC_OFFSET_MINUTES")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|m: &i32| m.abs() <= 14 * 60)
        .unwrap_or(420)
}

/// One answer given (or tried), for the log clients show.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LogEntry {
    pub id: i64,
    pub script_id: i64,
    pub script_name: String,
    pub thread_id: String,
    pub thread_name: String,
    /// What was answered.
    pub message_text: String,
    /// What was sent; empty while waiting, or when it failed.
    pub reply_text: String,
    pub at: i64,
    pub error: String,
}

/// Which chats a script answers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Every 1-1 chat.
    #[default]
    Users,
    /// 1-1 chats with people who are not friends.
    Strangers,
    /// 1-1 chats with friends.
    Friends,
    /// Group messages that name the account (`@`).
    Mentions,
    /// Only the chats listed in `threads`.
    Threads,
}

/// What a message must say.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Match {
    /// Anything.
    #[default]
    Any,
    /// One of the keywords, anywhere.
    Contains,
    /// Exactly one of the keywords (spaces, case and accents aside).
    Exact,
    /// Starts with one of the keywords.
    Starts,
}

/// When a script is on: on these days, from `from` to `to` (local time,
/// "HH:MM"; `to` before `from` runs past midnight).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    /// 0 Sunday … 6 Saturday; empty for every day.
    #[serde(default)]
    pub days: Vec<u8>,
    pub from: String,
    pub to: String,
}

/// A script.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    #[serde(default)]
    pub scope: Scope,
    /// Chats (gateway thread ids) for [`Scope::Threads`].
    #[serde(default)]
    pub threads: Vec<String>,
    /// Chats never answered by this script.
    #[serde(default)]
    pub except_threads: Vec<String>,
    #[serde(default, rename = "match")]
    pub matching: Match,
    /// One keyword per entry; ignored for [`Match::Any`].
    #[serde(default)]
    pub keywords: Vec<String>,
    /// `None`: always.
    #[serde(default)]
    pub schedule: Option<Schedule>,
    /// Only when the account has not written in the chat for this long
    /// (minutes); 0 for always.
    #[serde(default)]
    pub away_minutes: u32,
    /// The answer. `{ten}` is the sender's name, `{gio}` the time, `{ngay}`
    /// the date.
    pub reply: String,
    /// Seconds to wait before answering, as a person would.
    #[serde(default)]
    pub delay_seconds: u32,
    /// Minutes before this script answers the same chat again.
    #[serde(default = "default_cooldown")]
    pub cooldown_minutes: u32,
}

fn default_cooldown() -> u32 {
    60
}

/// A stored script.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Script {
    pub id: i64,
    pub name: String,
    pub enabled: bool,
    #[serde(flatten)]
    pub rule: Rule,
    pub updated_at: i64,
}

/// What is known of a new message when deciding.
#[derive(Clone, Debug, Default)]
pub struct Incoming {
    pub thread_id: String,
    pub group: bool,
    /// The sender is a friend of the account.
    pub friend: bool,
    /// A group message naming the account (or everyone).
    pub mentioned: bool,
    pub text: String,
    pub sender_name: String,
    /// When the account last wrote there itself (ms), 0 never; auto replies
    /// do not count.
    pub last_own_at: i64,
    /// Now (ms since the epoch, UTC).
    pub now: i64,
    /// Local time offset (minutes east of UTC).
    pub utc_offset_minutes: i32,
}

/// Why a script did not answer (shown when trying one).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Miss {
    Disabled,
    Scope,
    Excluded,
    Words,
    Schedule,
    NotAway,
}

impl Miss {
    pub fn text(self) -> &'static str {
        match self {
            Self::Disabled => "kịch bản đang tắt",
            Self::Scope => "cuộc trò chuyện không thuộc phạm vi của kịch bản",
            Self::Excluded => "cuộc trò chuyện này bị loại trừ",
            Self::Words => "tin nhắn không khớp từ khóa",
            Self::Schedule => "ngoài giờ áp dụng",
            Self::NotAway => "bạn vừa nhắn trong cuộc trò chuyện này",
        }
    }
}

/// "HH:MM" as minutes of the day.
pub fn minutes_of(hhmm: &str) -> Option<u32> {
    let (h, m) = hhmm.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// Day of the week (0 Sunday) and minute of the day, local time.
fn local_time(now: i64, offset_minutes: i32) -> (u8, u32) {
    let local = now.div_euclid(60_000) + i64::from(offset_minutes);
    let minute = local.rem_euclid(1440) as u32;
    let day = local.div_euclid(1440);
    // 1970-01-01 was a Thursday (4).
    ((day + 4).rem_euclid(7) as u8, minute)
}

fn in_schedule(s: &Schedule, now: i64, offset: i32) -> bool {
    let (Some(from), Some(to)) = (minutes_of(&s.from), minutes_of(&s.to)) else {
        return false;
    };
    let (day, minute) = local_time(now, offset);
    let day_ok = |d: u8| s.days.is_empty() || s.days.contains(&d);
    if from <= to {
        day_ok(day) && minute >= from && minute < to
    } else if minute >= from {
        // Past midnight: the evening belongs to its own day …
        day_ok(day)
    } else {
        // … and the small hours to the day before.
        minute < to && day_ok((day + 6) % 7)
    }
}

/// Lower case without accents, spaces squeezed.
fn plain(s: &str) -> String {
    zeta_zalo::fold(s)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn words_fit(rule: &Rule, text: &str) -> bool {
    if rule.matching == Match::Any {
        return true;
    }
    let text = plain(text);
    rule.keywords
        .iter()
        .map(|k| plain(k))
        .filter(|k| !k.is_empty())
        .any(|k| match rule.matching {
            Match::Contains => text.contains(&k),
            Match::Exact => text == k,
            Match::Starts => text.starts_with(&k),
            Match::Any => true,
        })
}

/// Whether a script answers a message, or why not.
pub fn check(enabled: bool, rule: &Rule, m: &Incoming) -> Result<(), Miss> {
    if !enabled {
        return Err(Miss::Disabled);
    }
    let scope = match rule.scope {
        Scope::Users => !m.group,
        Scope::Strangers => !m.group && !m.friend,
        Scope::Friends => !m.group && m.friend,
        Scope::Mentions => m.group && m.mentioned,
        Scope::Threads => rule.threads.contains(&m.thread_id),
    };
    if !scope {
        return Err(Miss::Scope);
    }
    if rule.except_threads.contains(&m.thread_id) {
        return Err(Miss::Excluded);
    }
    if !words_fit(rule, &m.text) {
        return Err(Miss::Words);
    }
    if let Some(s) = &rule.schedule {
        if !in_schedule(s, m.now, m.utc_offset_minutes) {
            return Err(Miss::Schedule);
        }
    }
    if rule.away_minutes > 0
        && m.last_own_at > 0
        && m.now - m.last_own_at < i64::from(rule.away_minutes) * 60_000
    {
        return Err(Miss::NotAway);
    }
    Ok(())
}

/// The first script that answers a message: it fits, and has not answered
/// that chat within its pause (`last` tells when a script last answered
/// there, ms, 0 never).
pub fn pick<'a>(
    scripts: &'a [Script],
    m: &Incoming,
    mut last: impl FnMut(i64) -> i64,
) -> Option<&'a Script> {
    scripts.iter().find(|s| {
        if check(s.enabled, &s.rule, m).is_err() {
            return false;
        }
        let at = last(s.id);
        let pause = i64::from(s.rule.cooldown_minutes.max(MIN_COOLDOWN_MINUTES)) * 60_000;
        at == 0 || m.now - at >= pause
    })
}

/// The answer with its variables filled: `{ten}` (or `{name}`), `{gio}`,
/// `{ngay}`.
pub fn render(reply: &str, m: &Incoming) -> String {
    let local = m.now.div_euclid(60_000) + i64::from(m.utc_offset_minutes);
    let minute = local.rem_euclid(1440);
    let (y, mo, d) = civil_from_days(local.div_euclid(1440));
    let name = if m.sender_name.trim().is_empty() {
        "bạn"
    } else {
        m.sender_name.trim()
    };
    reply
        .replace("{ten}", name)
        .replace("{name}", name)
        .replace("{gio}", &format!("{:02}:{:02}", minute / 60, minute % 60))
        .replace("{ngay}", &format!("{d:02}/{mo:02}/{y}"))
        .trim()
        .to_string()
}

/// Year, month, day of a day count since 1970-01-01 (Howard Hinnant's
/// algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// A script as clients send it: checked and tidied, or what is wrong.
pub fn checked(name: &str, mut rule: Rule) -> Result<(String, Rule), String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 80 {
        return Err("kịch bản cần tên (tối đa 80 ký tự)".into());
    }
    rule.reply = rule.reply.trim().to_string();
    if rule.reply.is_empty() || rule.reply.chars().count() > 2000 {
        return Err("nội dung trả lời trống hoặc quá dài".into());
    }
    rule.keywords = rule
        .keywords
        .iter()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .take(50)
        .collect();
    if rule.matching != Match::Any && rule.keywords.is_empty() {
        return Err("hãy nhập ít nhất một từ khóa".into());
    }
    if rule.scope == Scope::Threads && rule.threads.is_empty() {
        return Err("hãy chọn cuộc trò chuyện áp dụng".into());
    }
    if let Some(s) = &mut rule.schedule {
        if minutes_of(&s.from).is_none() || minutes_of(&s.to).is_none() || s.from == s.to {
            return Err("giờ áp dụng không hợp lệ".into());
        }
        s.days.retain(|d| *d < 7);
        s.days.sort_unstable();
        s.days.dedup();
    }
    rule.cooldown_minutes = rule
        .cooldown_minutes
        .clamp(MIN_COOLDOWN_MINUTES, 7 * 24 * 60);
    rule.delay_seconds = rule.delay_seconds.min(600);
    rule.away_minutes = rule.away_minutes.min(7 * 24 * 60);
    rule.threads.truncate(200);
    rule.except_threads.truncate(200);
    Ok((name.to_string(), rule))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-11-05 10:00 in Vietnam (UTC+7), a Thursday.
    const THURSDAY_10H: i64 = 1_793_847_600_000;

    fn incoming(text: &str) -> Incoming {
        Incoming {
            thread_id: "zalo:u:1".into(),
            text: text.into(),
            sender_name: "Lan".into(),
            now: THURSDAY_10H,
            utc_offset_minutes: 420,
            ..Incoming::default()
        }
    }

    fn rule() -> Rule {
        Rule {
            reply: "Chào {ten}!".into(),
            cooldown_minutes: 60,
            ..Rule::default()
        }
    }

    #[test]
    fn local_time_of_the_test_instant() {
        assert_eq!(local_time(THURSDAY_10H, 420), (4, 600));
        assert_eq!(render("{ngay} {gio}", &incoming("x")), "05/11/2026 10:00");
        assert_eq!(
            render("Chào {ten}, {name}", &incoming("x")),
            "Chào Lan, Lan"
        );
    }

    #[test]
    fn scopes() {
        let mut m = incoming("hi");
        assert_eq!(check(true, &rule(), &m), Ok(()));
        assert_eq!(check(false, &rule(), &m), Err(Miss::Disabled));
        let friends = Rule {
            scope: Scope::Friends,
            ..rule()
        };
        assert_eq!(check(true, &friends, &m), Err(Miss::Scope));
        m.friend = true;
        assert_eq!(check(true, &friends, &m), Ok(()));
        let strangers = Rule {
            scope: Scope::Strangers,
            ..rule()
        };
        assert_eq!(check(true, &strangers, &m), Err(Miss::Scope));
        // Groups only when the account is named.
        let mentions = Rule {
            scope: Scope::Mentions,
            ..rule()
        };
        m.group = true;
        assert_eq!(check(true, &rule(), &m), Err(Miss::Scope));
        assert_eq!(check(true, &mentions, &m), Err(Miss::Scope));
        m.mentioned = true;
        assert_eq!(check(true, &mentions, &m), Ok(()));
        let listed = Rule {
            scope: Scope::Threads,
            threads: vec!["zalo:u:1".into()],
            ..rule()
        };
        assert_eq!(check(true, &listed, &incoming("x")), Ok(()));
        let except = Rule {
            except_threads: vec!["zalo:u:1".into()],
            ..rule()
        };
        assert_eq!(check(true, &except, &incoming("x")), Err(Miss::Excluded));
    }

    #[test]
    fn keywords_ignore_case_accents_and_spaces() {
        let contains = Rule {
            matching: Match::Contains,
            keywords: vec!["giá".into(), "bao nhiêu".into()],
            ..rule()
        };
        assert!(check(true, &contains, &incoming("Cho mình hỏi GIA  bên bạn")).is_ok());
        assert!(check(true, &contains, &incoming("Cái này bao   nhieu?")).is_ok());
        assert_eq!(
            check(true, &contains, &incoming("Xin chào")),
            Err(Miss::Words)
        );
        let exact = Rule {
            matching: Match::Exact,
            keywords: vec!["Địa chỉ".into()],
            ..rule()
        };
        assert!(check(true, &exact, &incoming(" dia CHI ")).is_ok());
        assert!(check(true, &exact, &incoming("địa chỉ ở đâu")).is_err());
        let starts = Rule {
            matching: Match::Starts,
            keywords: vec!["/menu".into()],
            ..rule()
        };
        assert!(check(true, &starts, &incoming("/menu hôm nay")).is_ok());
    }

    #[test]
    fn schedules_and_away() {
        let hours = |from: &str, to: &str, days: Vec<u8>| Rule {
            schedule: Some(Schedule {
                days,
                from: from.into(),
                to: to.into(),
            }),
            ..rule()
        };
        // Thursday 10:00.
        assert!(check(true, &hours("08:00", "17:30", vec![]), &incoming("x")).is_ok());
        assert_eq!(
            check(true, &hours("17:30", "08:00", vec![]), &incoming("x")),
            Err(Miss::Schedule)
        );
        assert_eq!(
            check(true, &hours("08:00", "17:30", vec![0, 6]), &incoming("x")),
            Err(Miss::Schedule)
        );
        // Past midnight: 02:00 on Friday belongs to Thursday evening's window.
        let mut night = incoming("x");
        night.now = THURSDAY_10H + 16 * 3_600_000;
        assert_eq!(local_time(night.now, 420), (5, 120));
        assert!(check(true, &hours("22:00", "06:00", vec![4]), &night).is_ok());
        assert!(check(true, &hours("22:00", "06:00", vec![5]), &night).is_err());
        // Away: the account wrote 5 minutes ago.
        let away = Rule {
            away_minutes: 10,
            ..rule()
        };
        let mut m = incoming("x");
        m.last_own_at = m.now - 5 * 60_000;
        assert_eq!(check(true, &away, &m), Err(Miss::NotAway));
        m.last_own_at = m.now - 15 * 60_000;
        assert!(check(true, &away, &m).is_ok());
    }

    #[test]
    fn the_first_script_that_fits_and_rests_answers() {
        let script = |id: i64, enabled: bool, matching: Match, keywords: &[&str]| Script {
            id,
            name: format!("s{id}"),
            enabled,
            rule: Rule {
                matching,
                keywords: keywords.iter().map(|k| (*k).to_string()).collect(),
                ..rule()
            },
            updated_at: 0,
        };
        let scripts = [
            script(1, false, Match::Any, &[]),
            script(2, true, Match::Contains, &["giá"]),
            script(3, true, Match::Any, &[]),
        ];
        let m = incoming("giá bao nhiêu");
        assert_eq!(pick(&scripts, &m, |_| 0).map(|s| s.id), Some(2));
        assert_eq!(
            pick(&scripts, &incoming("chào"), |_| 0).map(|s| s.id),
            Some(3)
        );
        // Script 2 answered 10 minutes ago (pause 60): the next one answers.
        let ago = |id: i64| if id == 2 { m.now - 10 * 60_000 } else { 0 };
        assert_eq!(pick(&scripts, &m, ago).map(|s| s.id), Some(3));
        let all_rest = |_: i64| m.now - 60_000;
        assert_eq!(pick(&scripts, &m, all_rest), None);
    }

    #[test]
    fn scripts_are_checked() {
        assert!(checked(" ", rule()).is_err());
        assert!(checked(
            "A",
            Rule {
                reply: " ".into(),
                ..rule()
            }
        )
        .is_err());
        assert!(checked(
            "A",
            Rule {
                matching: Match::Contains,
                ..rule()
            }
        )
        .is_err());
        assert!(checked(
            "A",
            Rule {
                scope: Scope::Threads,
                ..rule()
            }
        )
        .is_err());
        let bad_hours = Rule {
            schedule: Some(Schedule {
                days: vec![],
                from: "25:00".into(),
                to: "08:00".into(),
            }),
            ..rule()
        };
        assert!(checked("A", bad_hours).is_err());
        let (name, r) = checked(
            " Vắng mặt ",
            Rule {
                cooldown_minutes: 0,
                delay_seconds: 9999,
                keywords: vec![" ".into(), " giá ".into()],
                schedule: Some(Schedule {
                    days: vec![6, 1, 1, 9],
                    from: "18:00".into(),
                    to: "08:00".into(),
                }),
                ..rule()
            },
        )
        .unwrap();
        assert_eq!(name, "Vắng mặt");
        assert_eq!((r.cooldown_minutes, r.delay_seconds), (1, 600));
        assert_eq!(r.keywords, ["giá"]);
        assert_eq!(r.schedule.unwrap().days, [1, 6]);
    }
}
