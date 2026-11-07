//! Ranking launcher entries for a typed query.
//!
//! Every whitespace-separated token must match something, so `web fire` finds
//! "Firefox Web Browser". Each token takes its best field (name beats
//! generic name and keywords beat comment), and the tokens add up. A caller
//! supplied boost (recently launched apps) is added on top and also orders the
//! empty query.

use crate::desktop::App;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    /// Index into the slice that was searched.
    pub index: usize,
    pub score: i32,
}

const NAME_EXACT: i32 = 1000;
const NAME_PREFIX: i32 = 400;
const NAME_WORD: i32 = 250;
const NAME_SUBSTRING: i32 = 120;
const NAME_FUZZY_MAX: i32 = 80;
const PROGRAM_EXACT: i32 = 300;
const PROGRAM_PREFIX: i32 = 150;
const PROGRAM_SUBSTRING: i32 = 40;
const TAG_WORD: i32 = 60;
const TAG_SUBSTRING: i32 = 35;
const COMMENT_SUBSTRING: i32 = 15;

/// Best matches first. An empty query returns every app, ordered by `boost`
/// and then by the order given (alphabetical, from the scan).
pub fn search(apps: &[App], query: &str, boost: &dyn Fn(&App) -> i32) -> Vec<Hit> {
    let query = query.to_lowercase();
    let tokens: Vec<&str> = query.split_whitespace().collect();

    let mut hits: Vec<Hit> = apps
        .iter()
        .enumerate()
        .filter_map(|(index, app)| {
            let mut score = boost(app);
            for t in &tokens {
                score += token_score(app, t)?;
            }
            Some(Hit { index, score })
        })
        .collect();

    if tokens.is_empty() {
        // Nothing typed: keep the scan's (alphabetical) order among equals.
        hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.index.cmp(&b.index)));
        return hits;
    }
    hits.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| apps[a.index].name.chars().count().cmp(&apps[b.index].name.chars().count()))
            .then_with(|| apps[a.index].name_lc.cmp(&apps[b.index].name_lc))
            .then_with(|| a.index.cmp(&b.index))
    });
    hits
}

/// The best way `t` (already lowercase) matches `app`; `None` when it does not.
fn token_score(app: &App, t: &str) -> Option<i32> {
    let mut best: Option<i32> = None;
    let mut take = |s: i32| best = Some(best.map_or(s, |b| b.max(s)));

    if app.name_lc == t {
        take(NAME_EXACT);
    } else if app.name_lc.starts_with(t) {
        take(NAME_PREFIX);
    } else if word_start(&app.name_lc, t) {
        take(NAME_WORD);
    } else if app.name_lc.contains(t) {
        take(NAME_SUBSTRING);
    } else if let Some(s) = fuzzy(&app.name_lc, t) {
        take(s);
    }

    if app.program_lc == t {
        take(PROGRAM_EXACT);
    } else if app.program_lc.starts_with(t) {
        take(PROGRAM_PREFIX);
    } else if app.program_lc.contains(t) {
        take(PROGRAM_SUBSTRING);
    }

    for tag in app.keywords_lc.iter().map(String::as_str).chain(std::iter::once(app.generic_lc.as_str())) {
        if tag.is_empty() {
            continue;
        }
        if word_start(tag, t) {
            take(TAG_WORD);
        } else if tag.contains(t) {
            take(TAG_SUBSTRING);
        }
    }

    if app.comment_lc.contains(t) {
        take(COMMENT_SUBSTRING);
    }
    best
}

/// Does `t` begin a word of `hay`? Words start at the beginning and after any
/// character that is not a letter or digit (`Visual Studio Code`, `gnome-tweaks`).
fn word_start(hay: &str, t: &str) -> bool {
    let mut prev_alnum = false;
    for (i, c) in hay.char_indices() {
        if !prev_alnum && hay[i..].starts_with(t) {
            return true;
        }
        prev_alnum = c.is_alphanumeric();
    }
    false
}

/// `t` as a subsequence of `hay` (`fx` in `firefox`). Rewards runs and an
/// early start; never beats a plain substring. Needs two characters, because
/// a single letter is a subsequence of half the list.
fn fuzzy(hay: &str, t: &str) -> Option<i32> {
    if t.chars().count() < 2 {
        return None;
    }
    let mut hay_chars = hay.chars().enumerate();
    let mut score = 20;
    let mut prev: Option<usize> = None;
    for (n, tc) in t.chars().enumerate() {
        let (pos, _) = hay_chars.find(|&(_, hc)| hc == tc)?;
        if prev.is_some_and(|p| p + 1 == pos) {
            score += 8;
        }
        if n == 0 && pos < 3 {
            score += 10;
        }
        prev = Some(pos);
    }
    Some(score.min(NAME_FUZZY_MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str, program: &str) -> App {
        App {
            id: format!("{program}.desktop"),
            name: name.into(),
            generic_name: String::new(),
            comment: String::new(),
            keywords: vec![],
            categories: vec![],
            icon: String::new(),
            exec: program.into(),
            path: Default::default(),
            program: program.into(),
            name_lc: String::new(),
            generic_lc: String::new(),
            comment_lc: String::new(),
            keywords_lc: vec![],
            program_lc: String::new(),
        }
        .index()
    }

    fn names(apps: &[App], hits: &[Hit]) -> Vec<String> {
        hits.iter().map(|h| apps[h.index].name.clone()).collect()
    }

    fn q(apps: &[App], query: &str) -> Vec<String> {
        names(apps, &search(apps, query, &|_| 0))
    }

    #[test]
    fn name_matches_rank_exact_prefix_word_substring_fuzzy() {
        let apps = [
            app("Fox Editor", "foxed"),         // prefix of "fox"? no: exact is "fox"
            app("fox", "x1"),                   // exact
            app("Red Fox Viewer", "x2"),        // word start
            app("Firefox", "firefox-bin-x3"),   // substring
            app("Flat Orange Xylophone", "x4"), // fuzzy f-o-x
        ];
        assert_eq!(q(&apps, "fox"), ["fox", "Fox Editor", "Red Fox Viewer", "Firefox", "Flat Orange Xylophone"]);
    }

    #[test]
    fn every_token_must_match_and_they_may_hit_different_words() {
        let apps = [app("Firefox Web Browser", "firefox"), app("Web Inspector", "inspector"), app("Terminal", "foot")];
        assert_eq!(q(&apps, "web fire"), ["Firefox Web Browser"]);
        assert_eq!(q(&apps, "web"), ["Web Inspector", "Firefox Web Browser"]);
        assert!(q(&apps, "web terminal").is_empty());
    }

    #[test]
    fn what_you_would_type_in_a_shell_finds_the_app() {
        let apps = [app("Visual Studio Code", "code"), app("Codec Pack", "codecs")];
        assert_eq!(q(&apps, "code"), ["Codec Pack", "Visual Studio Code"]);
        // exact program beats a name prefix only through the program score, so
        // check the score directly rather than rely on tie-breaks
        let hits = search(&apps, "code", &|_| 0);
        let vsc = hits.iter().find(|h| apps[h.index].program == "code").unwrap();
        assert_eq!(vsc.score, NAME_WORD.max(PROGRAM_EXACT));
    }

    #[test]
    fn keywords_generic_name_and_comment_are_weaker_than_the_name() {
        let mut files = app("Files", "nautilus");
        files.keywords = vec!["folder".into(), "disk".into()];
        files.generic_name = "File Manager".into();
        files.comment = "Access and organise your documents".into();
        let files = files.index();
        let apps = [files, app("Folder Sync", "foldersync")];
        assert_eq!(q(&apps, "folder"), ["Folder Sync", "Files"]);
        assert_eq!(q(&apps, "manager"), ["Files"]);
        assert_eq!(q(&apps, "organise"), ["Files"]);
        assert_eq!(q(&apps, "nautilus"), ["Files"]);
    }

    #[test]
    fn one_letter_is_not_fuzzy() {
        let apps = [app("Alpha", "a"), app("Beta Zeta", "b"), app("Omega", "o")];
        // "z" is a substring of Beta Zeta, a subsequence of nothing else
        assert_eq!(q(&apps, "z"), ["Beta Zeta"]);
        // "ga" is a subsequence of "omega"? o-m-e-g-a yes, but not of "alpha"
        assert_eq!(q(&apps, "ga"), ["Omega"]);
    }

    #[test]
    fn fuzzy_finds_abbreviations_but_never_outranks_a_substring() {
        let apps = [app("Firefox", "ff"), app("Fair Offer Xchange", "fox")];
        assert_eq!(q(&apps, "fx"), ["Firefox", "Fair Offer Xchange"]);
        let a = [app("Fair Offer Xchange", "z1"), app("Soft Fox", "z2")];
        assert_eq!(q(&a, "fox"), ["Soft Fox", "Fair Offer Xchange"]);
    }

    #[test]
    fn empty_query_keeps_scan_order_and_a_boost_reorders_it() {
        let apps = [app("Alpha", "a"), app("Beta", "b"), app("Gamma", "g")];
        assert_eq!(q(&apps, ""), ["Alpha", "Beta", "Gamma"]);
        assert_eq!(q(&apps, "   "), ["Alpha", "Beta", "Gamma"]);
        let boosted = search(&apps, "", &|a| if a.name == "Gamma" { 50 } else { 0 });
        assert_eq!(names(&apps, &boosted), ["Gamma", "Alpha", "Beta"]);
    }

    #[test]
    fn a_boost_lifts_a_weaker_match_but_not_past_an_exact_one() {
        let apps = [app("Terminal", "foot"), app("Termite", "termite"), app("Term", "term")];
        let boosted = search(&apps, "term", &|a| if a.name == "Terminal" { 200 } else { 0 });
        assert_eq!(names(&apps, &boosted), ["Term", "Terminal", "Termite"]);
    }

    #[test]
    fn ties_prefer_the_shorter_name_then_alphabetical() {
        let apps = [app("Zed Pro Max", "z1"), app("Zed Pro", "z2"), app("Zed Aaa", "z3")];
        assert_eq!(q(&apps, "zed"), ["Zed Aaa", "Zed Pro", "Zed Pro Max"]);
    }

    #[test]
    fn case_and_unicode_do_not_matter() {
        let apps = [app("Café Menu", "cafe"), app("Ünïcode Tool", "uni")];
        assert_eq!(q(&apps, "CAFÉ"), ["Café Menu"]);
        assert_eq!(q(&apps, "ünï"), ["Ünïcode Tool"]);
        assert_eq!(q(&apps, "TOOL"), ["Ünïcode Tool"]);
    }

    #[test]
    fn word_starts_include_punctuation_boundaries() {
        assert!(word_start("gnome-tweaks", "tweaks"));
        assert!(word_start("visual studio code", "studio"));
        assert!(!word_start("firefox", "fox"));
        assert!(!word_start("", "a"));
    }

    #[test]
    fn nothing_matches_nothing() {
        let apps = [app("Alpha", "a")];
        assert!(q(&apps, "zzz").is_empty());
        assert!(q(&[], "a").is_empty());
    }
}
