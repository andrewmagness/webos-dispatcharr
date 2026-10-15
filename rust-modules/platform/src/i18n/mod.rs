//! Immutable per-launch localization. Catalog source is JSON; generated accessors are typed.
use icu_decimal::{input::Decimal, DecimalFormatter};
use icu_locale::Locale;
use icu_plurals::{PluralCategory, PluralRules};
use serde::{Deserialize, Deserializer, Serialize};
use std::sync::OnceLock;

pub const CONTRIBUTE_URL: &str =
    "https://github.com/GLinnik21/plx-native/blob/main/docs/localization.md";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Preference {
    #[default]
    System,
    En,
    Es,
    Be,
    Fr,
}
/// Every language the app ships, `System` excluded — the grid a text-fit test sums over.
#[cfg(any(test, feature = "test-support"))]
pub const SHIPPED: [Preference; 4] =
    [Preference::En, Preference::Es, Preference::Be, Preference::Fr];
impl<'de> Deserialize<'de> for Preference {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        Ok(Self::from_tag(v.as_str().unwrap_or("system")))
    }
}
impl Preference {
    pub fn from_tag(s: &str) -> Self {
        match s {
            "en" => Self::En,
            "es" => Self::Es,
            "be" => Self::Be,
            "fr" => Self::Fr,
            _ => Self::System,
        }
    }
    pub fn tag(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::En => "en",
            Self::Es => "es",
            Self::Be => "be",
            Self::Fr => "fr",
        }
    }
    /// Serde's `skip_serializing_if`: System is what an absent preference already means.
    pub fn is_system(&self) -> bool {
        *self == Self::System
    }
    pub fn native_name(self) -> &'static str {
        match self {
            Self::System => msg::core_system_default(),
            Self::En => "English",
            Self::Es => "Español",
            Self::Be => "Беларуская",
            Self::Fr => "Français",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Language {
    En,
    Es,
    Be,
    Fr,
    /// Generated expanded labels; selectable only by the host simulator.
    #[cfg_attr(not(any(test, feature = "test-support", feature = "hostsim")), allow(dead_code))]
    Pseudo,
}
impl Language {
    pub fn tag(self) -> &'static str {
        match self {
            Self::En | Self::Pseudo => "en",
            Self::Es => "es",
            Self::Be => "be",
            Self::Fr => "fr",
        }
    }
    fn regional(self) -> &'static str {
        match self {
            Self::En | Self::Pseudo => "en-US",
            Self::Es => "es-ES",
            Self::Be => "be-BY",
            Self::Fr => "fr-FR",
        }
    }
    fn parse(s: &str) -> Self {
        let Ok(l) = s.parse::<Locale>() else {
            return Self::En;
        };
        match l.id.language.as_str() {
            "es" if l.id.script.is_none_or(|s| s.as_str() == "Latn") => Self::Es,
            "be" if l.id.script.is_none_or(|s| s.as_str() == "Cyrl") => Self::Be,
            "fr" if l.id.script.is_none_or(|s| s.as_str() == "Latn") => Self::Fr,
            _ => Self::En,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Clock {
    Locale,
    H12,
    H24,
}

pub struct LocaleContext {
    preference: Preference,
    language: Language,
    format_locale: String,
    clock: Clock,
    plural: PluralRules,
    decimal: DecimalFormatter,
    date: icu_datetime::FixedCalendarDateTimeFormatter<
        icu_calendar::Gregorian,
        icu_datetime::fieldsets::YMD,
    >,
    /// Hours and minutes, on the hour cycle the television's clock setting names (`clock`).
    time: icu_datetime::NoCalendarFormatter<icu_datetime::fieldsets::T>,
}
impl LocaleContext {
    /// Exercise expanded catalogs without changing the process-wide locale or environment.
    #[cfg(any(test, feature = "test-support"))]
    pub fn pseudo_for_test() -> Self {
        let mut context = Self::resolve(Preference::En, None, None, None, None);
        context.language = Language::Pseudo;
        context
    }

    pub fn resolve(
        preference: Preference,
        ui: Option<&str>,
        fmt: Option<&str>,
        clock: Option<&str>,
        env: Option<&str>,
    ) -> Self {
        let selected = if preference == Preference::System {
            ui.and_then(normalize)
                .or_else(|| env.and_then(normalize))
                .unwrap_or_else(|| "en".into())
        } else {
            preference.tag().into()
        };
        let language = Language::parse(&selected);
        let format_locale = fmt
            .and_then(normalize)
            .or_else(|| {
                let selected = selected.parse::<Locale>().ok()?;
                (selected.id.language.as_str() == language.tag() && selected.id.region.is_some())
                    .then(|| selected.to_string())
            })
            .unwrap_or_else(|| language.regional().into());
        let locale = format_locale.parse::<Locale>().expect("validated locale");
        let decimal = DecimalFormatter::try_new(locale.clone().into(), Default::default())
            .unwrap_or_else(|_| {
                DecimalFormatter::try_new(Default::default(), Default::default())
                    .expect("compiled English number data")
            });
        let clock = match clock {
            Some("12") => Clock::H12,
            Some("24") => Clock::H24,
            _ => Clock::Locale,
        };
        let time = time_formatter(&locale, clock);
        let date = icu_datetime::FixedCalendarDateTimeFormatter::try_new(
            locale.into(),
            icu_datetime::fieldsets::YMD::short()
                .with_year_style(icu_datetime::options::YearStyle::Full),
        )
        .unwrap_or_else(|_| {
            icu_datetime::FixedCalendarDateTimeFormatter::try_new(
                Default::default(),
                icu_datetime::fieldsets::YMD::short()
                    .with_year_style(icu_datetime::options::YearStyle::Full),
            )
            .expect("compiled English date data")
        });
        let plural =
            PluralRules::try_new_cardinal(language.tag().parse::<Locale>().unwrap().into())
                .expect("compiled catalog plural data");
        Self {
            preference,
            language,
            format_locale,
            clock,
            plural,
            decimal,
            date,
            time,
        }
    }
    pub fn preference(&self) -> Preference {
        self.preference
    }
    pub fn language(&self) -> Language {
        self.language
    }
    pub fn format_locale(&self) -> &str {
        &self.format_locale
    }
    pub fn clock(&self) -> Clock {
        self.clock
    }
    pub fn plural(&self, n: i64) -> PluralCategory {
        self.plural.category_for(n.unsigned_abs())
    }
    pub fn number(&self, n: i64) -> String {
        self.decimal.format(&Decimal::from(n)).to_string()
    }
    pub fn decimal(&self, n: i64, scale: i16) -> String {
        let mut d = Decimal::from(n);
        d.multiply_pow10(-scale);
        d.pad_end(-scale);
        self.decimal.format(&d).to_string()
    }
    /// A time of day (`hour` 0–23), in this locale's form and on the clock setting's hour cycle:
    /// `21:05`, `9:05 PM`, `21 h 05`. `None` for an hour or minute out of range.
    pub fn time(&self, hour: u8, minute: u8) -> Option<String> {
        let t = icu_datetime::input::Time::try_new(hour, minute, 0, 0).ok()?;
        Some(self.time.format(&t).to_string())
    }
    pub fn date(&self, year: i32, month: u8, day: u8) -> Option<String> {
        let d = icu_calendar::Date::try_new_gregorian(year, month, day).ok()?;
        Some(self.date.format(&d).to_string())
    }
}
/// The hour-and-minute formatter for `locale`, with the hour cycle `clock` forces (none for
/// `Clock::Locale`, which keeps the locale's own).
fn time_formatter(locale: &Locale, clock: Clock) -> icu_datetime::NoCalendarFormatter<icu_datetime::fieldsets::T> {
    use icu_datetime::preferences::HourCycle;
    let mut prefs: icu_datetime::DateTimeFormatterPreferences = locale.clone().into();
    prefs.hour_cycle = match clock {
        Clock::Locale => None,
        Clock::H12 => Some(HourCycle::H12),
        Clock::H24 => Some(HourCycle::H23),
    };
    icu_datetime::NoCalendarFormatter::try_new(prefs, icu_datetime::fieldsets::T::hm()).unwrap_or_else(|_| {
        icu_datetime::NoCalendarFormatter::try_new(Default::default(), icu_datetime::fieldsets::T::hm())
            .expect("compiled English time data")
    })
}

/// The local hour and minute of Unix instant `unix`, in the process's time zone (`localtime_r`:
/// `TZ`, else `/etc/localtime`). `None` when the C library cannot place it.
///
/// Verified on the host only. Whether a native app under webOS's jail sees the zone the
/// television's own settings name (rather than UTC) has not been checked on a set yet.
pub fn local_hour_minute(unix: i64) -> Option<(u8, u8)> {
    let t = libc::time_t::try_from(unix).ok()?;
    // SAFETY: `localtime_r` writes only the `tm` it is handed and reads only `t`.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&t, &mut tm).is_null() };
    ok.then(|| (tm.tm_hour as u8, tm.tm_min as u8))
}

/// Unix instant `unix` as a time of day on this television's clock, in the current locale: what a
/// guide's axis and a programme's times read. Empty when it cannot be placed.
pub fn wall_time(unix: i64) -> String {
    local_hour_minute(unix).and_then(|(h, m)| current().time(h, m)).unwrap_or_default()
}

/// Reject control bytes and invalid tags before they can become HTTP headers. POSIX suffixes
/// are only used at this input boundary; callers retain canonical BCP-47 tags.
pub fn normalize(s: &str) -> Option<String> {
    if !s.is_ascii() || s.bytes().any(|b| b.is_ascii_control()) {
        return None;
    }
    let base = s.trim().split(['.', '@']).next()?.replace('_', "-");
    if base.eq_ignore_ascii_case("C") || base.eq_ignore_ascii_case("POSIX") {
        return None;
    }
    let l = base.parse::<Locale>().ok()?;
    (l.id.language.as_str() != "und").then(|| l.to_string())
}
/// `webOS 4.10.2`, or the UI language's "webOS unknown" read-out when the firmware file could not
/// be read — the release is the one field a stranger's report needs, and "unknown" is the honest
/// reading of an empty one rather than a plausible default. Shared by the support lines of the
/// failure read-out and of the sign-in report.
pub fn webos_release_line(info: &crate::tv::device::Info) -> String {
    if info.major == 0 {
        msg::browse_diagnostics_unknown_os().to_string()
    } else {
        format!("webOS {}", info.release)
    }
}
static CURRENT: OnceLock<LocaleContext> = OnceLock::new();
static SAVED_PREFERENCE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// The confirmed next-launch setting. Reading it never touches credential storage.
pub fn saved_preference() -> Preference {
    match SAVED_PREFERENCE.load(std::sync::atomic::Ordering::Acquire) {
        1 => Preference::En,
        2 => Preference::Es,
        3 => Preference::Be,
        4 => Preference::Fr,
        _ => Preference::System,
    }
}

pub fn set_saved_preference(value: Preference) {
    let value = match value {
        Preference::System => 0,
        Preference::En => 1,
        Preference::Es => 2,
        Preference::Be => 3,
        Preference::Fr => 4,
    };
    SAVED_PREFERENCE.store(value, std::sync::atomic::Ordering::Release);
}

#[cfg(any(test, feature = "test-support"))]
pub fn saved_preference_for_test(value: Preference) -> Preference {
    plx_base::testlock::assert_held("saved language preference");
    let previous = saved_preference();
    set_saved_preference(value);
    previous
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static THREAD_LOCALE: std::cell::Cell<Option<&'static LocaleContext>> = const { std::cell::Cell::new(None) };
}

/// Resolve every catalog accessor on THIS test thread in the expanded pseudo-locale until the
/// guard drops. Tests run in parallel, so the process-wide [`current`] must stay English for every
/// other test; a thread-local override is what lets one test draw a whole screen in `[!! … !!]`.
#[cfg(any(test, feature = "test-support"))]
pub fn pseudo_on_this_thread_for_test() -> ThreadLocaleGuard {
    static PSEUDO: OnceLock<LocaleContext> = OnceLock::new();
    let pseudo = PSEUDO.get_or_init(LocaleContext::pseudo_for_test);
    ThreadLocaleGuard(THREAD_LOCALE.with(|slot| slot.replace(Some(pseudo))))
}

/// Resolve every catalog accessor on THIS test thread in one shipped UI language until the guard
/// drops — the pseudo-locale's thread-local door, for tests that measure real translations.
#[cfg(any(test, feature = "test-support"))]
pub fn language_on_this_thread_for_test(preference: Preference) -> ThreadLocaleGuard {
    static EN: OnceLock<LocaleContext> = OnceLock::new();
    static ES: OnceLock<LocaleContext> = OnceLock::new();
    static BE: OnceLock<LocaleContext> = OnceLock::new();
    static FR: OnceLock<LocaleContext> = OnceLock::new();
    let slot = match preference {
        Preference::Es => &ES,
        Preference::Be => &BE,
        Preference::Fr => &FR,
        Preference::En | Preference::System => &EN,
    };
    let cx = slot.get_or_init(|| LocaleContext::resolve(preference, None, None, None, None));
    ThreadLocaleGuard(THREAD_LOCALE.with(|slot| slot.replace(Some(cx))))
}

#[cfg(any(test, feature = "test-support"))]
pub struct ThreadLocaleGuard(Option<&'static LocaleContext>);

#[cfg(any(test, feature = "test-support"))]
impl Drop for ThreadLocaleGuard {
    fn drop(&mut self) {
        THREAD_LOCALE.with(|slot| slot.set(self.0));
    }
}

pub fn current() -> &'static LocaleContext {
    #[cfg(any(test, feature = "test-support"))]
    {
        if let Some(cx) = THREAD_LOCALE.with(std::cell::Cell::get) {
            return cx;
        }
        CURRENT.get_or_init(|| LocaleContext::resolve(Preference::En, None, None, None, None))
    }
    #[cfg(not(any(test, feature = "test-support")))]
    {
        CURRENT
            .get()
            .expect("locale initialized before any screen or Plex request")
    }
}
// Always compiled, though no test calls it: the application's boot calls it from code that is
// itself `cfg(not(test))`, and that code builds against this crate WITH `test-support` whenever
// cargo unifies a dependent's dev-dependency features into a non-test build of the dependent.
pub fn initialize(preference: Preference, controlled: bool) {
    set_saved_preference(preference);
    // Record/replay must not depend on the TV or process environment. The preference is part
    // of the captured session; System resolves to English and formatting is fixed in both modes.
    if controlled {
        let _ = CURRENT.set(LocaleContext::resolve(
            preference,
            Some("en-US"),
            Some("en-US"),
            Some("24"),
            None,
        ));
        return;
    }
    let info = platform_locale();
    let env = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|s| !s.is_empty()));
    let (ui, fmt, clock) = info
        .as_ref()
        .map(|v| (v.ui.as_deref(), v.fmt.as_deref(), v.clock.as_deref()))
        .unwrap_or_default();
    #[cfg(feature = "hostsim")]
    let simulated_format = std::env::var("DISPATCHARRTV_FORMAT_LOCALE").ok();
    #[cfg(feature = "hostsim")]
    let fmt = simulated_format.as_deref().or(fmt);
    #[allow(unused_mut)]
    let mut cx = LocaleContext::resolve(preference, ui, fmt, clock, env.as_deref());
    #[cfg(feature = "hostsim")]
    if let Ok(forced) = std::env::var("DISPATCHARRTV_LOCALE") {
        cx = LocaleContext::resolve(
            Preference::from_tag(&forced),
            Some(&forced),
            fmt,
            clock,
            None,
        );
        if forced == "qps-ploc" {
            cx.language = Language::Pseudo;
        }
    }
    plx_base::eventlog::log(&format!(
        "locale: source={} preference={} ui={} format={} clock={:?}",
        if info.is_some() {
            "settings"
        } else if env.is_some() {
            "environment"
        } else {
            "fallback"
        },
        cx.preference().tag(),
        cx.language().tag(),
        cx.format_locale(),
        cx.clock()
    ));
    if CURRENT.set(cx).is_err() {
        plx_base::eventlog::log("locale: initialization already completed");
    }
}
#[derive(Default)]
struct SystemLocale {
    ui: Option<String>,
    fmt: Option<String>,
    clock: Option<String>,
}
fn parse_reply(raw: &str) -> Option<SystemLocale> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    if v["returnValue"].as_bool() != Some(true) {
        return None;
    }
    let info = &v["settings"]["localeInfo"];
    if !info.is_object() {
        return None;
    }
    Some(SystemLocale {
        ui: info["locales"]["UI"].as_str().and_then(normalize),
        fmt: info["locales"]["FMT"].as_str().and_then(normalize),
        clock: info["clock"].as_str().map(str::to_string),
    })
}
fn platform_locale() -> Option<SystemLocale> {
    match crate::tv::system_locale() {
        crate::tv::LocaleReply::NoPlatform => None,
        crate::tv::LocaleReply::Reply(raw) => {
            let info = parse_reply(&raw);
            if info.is_none() {
                plx_base::eventlog::log("locale: settings refused or returned malformed localeInfo");
            }
            info
        }
        crate::tv::LocaleReply::Unavailable => {
            plx_base::eventlog::log("locale: settings unavailable; using fallback");
            None
        }
    }
}

// Every generated key has str/CStr and explicit-context variants; not every consumer needs all
// four. Keep the allowance on this generated API rather than suppressing handwritten warnings.
#[allow(dead_code)]
pub mod msg {
    include!(concat!(env!("OUT_DIR"), "/messages.rs"));
}
#[cfg(test)]
#[allow(dead_code)]
#[path = "../../build_support/catalog.rs"]
mod catalog_tests;
#[cfg(test)]
mod tests;
