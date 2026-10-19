# Help translate Dispatcharr TV

Dispatcharr TV's interface is available in English, Spanish, French and Belarusian (official Cyrillic
orthography). Help improve a translation or contribute another language through a pull request
at https://github.com/andrewmagness/webos-dispatcharr.

## Translate a message

Catalogs live in `locales/en`, `locales/es`, `locales/fr` and `locales/be`, split by UI area. English is the
source: each scoped, stable key has a `value`, translator `description`, and optional typed
`args`. The other languages contain the same keys with translated string or plural values.
The compiler combines each language's files; file names organize ownership, not message identity.

```json
{
  "dispatcharr.setup.channels": {
    "value": {"one": "{count} channel", "other": "{count} channels"},
    "args": {"count": "i64"},
    "description": "Under the connected verdict: how many channels the account can watch"
  }
}
```

Spanish would provide:

```json
{
  "dispatcharr.setup.channels": {
    "one": "{count} canal",
    "many": "{count} canales",
    "other": "{count} canales"
  }
}
```

Keep placeholder names unchanged, but reorder them to suit the language. Translate the complete
phrase. Never translate protocol identifiers, URLs, brand names, or channel names and programme titles your server returns.
Double braces (`{{` and `}}`) render literal braces. A placeholder accepts `str` or `i64`;
integers are formatted with the TV's regional settings. A plural message's `count` selects a
CLDR cardinal category for the UI language. Other arguments must appear in every form; count
may be omitted when the form spells it out. Belarusian needs `one`, `few`, `many`, and `other`;
Spanish and French need `one`, `many`, and `other`; English needs `one` and `other`.

Use readable, natural wording. Belarusian switch labels use `Укл.` and `Выкл.`.
Third-party license texts stay verbatim. Screenshots of the affected screen help reviewers assess
context and wrapping; do not include API keys, private server addresses or your channel line-up.

## Check and submit

Run `make check` to validate catalogs and run the host checks. Let it finish before starting
another build in the same checkout: its structure-gate tests temporarily modify source files. Every build rejects duplicate,
missing or unknown keys, invalid placeholders, and incomplete plural forms. The generated
accessors also make unknown message keys and wrong argument types compilation errors. Source
wording can change without renaming its key; change a key only when its meaning changes.

Some slots have a fixed width and line count, and a translation that overflows one ends in `…`.
Host tests measure each shipped language with the TV's own glyph widths and fail when such a line
would not fit — Setup's labels (`screens/src/setup_tests.rs`), the player's failure read-out
(`appkit/src/player_hud.rs`) and the clock notice (`src/app/clock_notice.rs`) among them. When one
of these tests fails, shorten the wording; do not abbreviate it past readability.

Review the affected screens with the desktop simulator. `DISPATCHARRTV_LOCALE=es`, `be` or `en`
selects a language for a simulator launch; `DISPATCHARRTV_FORMAT_LOCALE=de-DE` exercises separate
regional formatting. `DISPATCHARRTV_LOCALE=qps-ploc` expands and accents text to reveal cramped
layouts. These environment overrides do not operate in the shipping TV build.

Open a pull request describing which language and screens changed, and how you checked the
wording and layout. Translations ship with app releases; the app does not download language packs.
New translations and changes remain subject to language review.

### Who reviews a translation

A translation is accepted when a native speaker of that language has read it. Machine or AI-drafted
translations are welcome as a starting point, but say so in the pull request and name who reviewed
the wording (the author, or someone they asked). A pull request from a non-native speaker waits
for a native reviewer before it is merged.

The reviewer does not need to build anything. Send them the catalog files in `locales/<tag>/` and
screenshots of the affected screens (desktop simulator with `DISPATCHARRTV_LOCALE=<tag>`, or the TV),
so they can see each string in its place: the same words read differently in a button, a menu and
a sentence. Note any wording they changed and why.

## Adding a language

Add a catalog directory using its BCP-47 language tag, translating all source keys. This currently
also requires a code change: extend the compiler's locale list and plural requirements, the
runtime language and preference enums, and tests. Select CLDR plural rules
for that language, add launcher metadata, and verify every character in both bundled font faces.
The current renderer supports the shipped Latin and Cyrillic languages; Arabic/Hebrew and other
scripts requiring shaping or bidirectional layout need renderer work before being offered.

## Runtime behavior

The app follows the television's UI language; there is no language setting inside the app. At
boot, the native Settings Service supplies `localeInfo.locales.UI`, `locales.FMT` and `clock`. UI
resolution is the TV's UI language, then the process locale, then English; unsupported UI languages
use English. Missing formatting settings use the resolved UI language's regional default. Numeric
Gregorian dates, numbers and the guide's clock times use the formatting locale and the TV's 12/24-hour
setting. Locale data and catalogs are bundled, so translating the UI requires no network.

Channel names and programme titles come from your Dispatcharr server as it stores them; the UI
language does not change them. Launcher metadata (`pkg/resources/<locale>/appinfo.json`) follows
webOS's system language.

## History

The catalog system, the four languages and the glyph-width tests were built in the PlxNative code
this project grew from and verified there on 2026-09-27, including on a webOS 4.5 television that
queried the Settings Service. Its screens were since replaced; the Dispatcharr screens' strings live
in `locales/<tag>/dispatcharr.json`.
