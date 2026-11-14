//! Which routine a phrase asks for: the same words, in any order.
//!
//! The rule is Atticus's, and it is deliberately blunt. A phrase is lowered,
//! cut into letters-and-digits words and stripped of the filler words
//! ([`STOP`]); two phrases are the same request when those word SETS are equal.
//! "open the notes app" and "notes app, open it please" are one routine;
//! "open notes" and "open notes and mail" are two. There is no scoring and no
//! partial match: a routine that runs on a near miss runs the wrong thing on a
//! machine the operator is signed into, and a miss only costs the agent doing
//! the task the long way.
//!
//! Direction words are NOT filler: "turn the volume up" and "turn the volume
//! down" must stay different requests, and so must "on" and "off".
//!
//! "Any order" stops at a word that says which way something goes ([`DIRECTION`]:
//! to, from, into, ...). The phrase is cut there and each side matches as its own
//! set, so "copy backup files to desktop" never runs for "copy desktop files to
//! backup": the same words, the opposite task.

use std::collections::BTreeSet;

use super::Routine;

/// Words that carry no request. Atticus's list less `on`, `in`, `what` and
/// `how`, which can change what a phrase means ("turn it on", "show me how").
/// `this` and `that` stay here for matching; [`is_deictic`] is what refuses to
/// LEARN them.
pub const STOP: &[&str] = &[
    "a", "an", "the", "to", "of", "and", "or", "for", "my", "me", "i", "you", "it", "is", "be",
    "do", "can", "could", "would", "please", "with", "at", "this", "that",
];

/// The words of `phrase` that name the request, as a set.
#[must_use]
pub fn word_set(phrase: &str) -> BTreeSet<String> {
    phrase
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty() && !STOP.contains(w))
        .map(str::to_string)
        .collect()
}

/// Words that say which way a thing goes. They are filler or ordinary words
/// elsewhere, but here the words on each side of one must stay on that side:
/// "copy backup files to desktop" is not "copy desktop files to backup", and
/// a routine that ran on the second would copy the wrong way round.
pub const DIRECTION: &[&str] = &[
    "to", "from", "into", "onto", "over", "under", "before", "after",
];

/// The phrase's identity. Two phrases with the same key are the same request.
///
/// With no direction word ([`DIRECTION`]) it is the word set, sorted and
/// joined: any order. With one, the phrase is cut at each direction word and
/// each side is its own sorted word set, so the words may come in any order
/// WITHIN a side and never cross a direction word.
#[must_use]
pub fn key(phrase: &str) -> String {
    let lowered = phrase.to_lowercase();
    let mut sides: Vec<BTreeSet<&str>> = vec![BTreeSet::new()];
    let mut directions: Vec<&str> = Vec::new();
    for word in lowered.split(|c: char| !c.is_alphanumeric()) {
        if word.is_empty() {
            continue;
        }
        if let Some(direction) = DIRECTION.iter().find(|d| **d == word) {
            directions.push(direction);
            sides.push(BTreeSet::new());
        } else if !STOP.contains(&word) {
            if let Some(side) = sides.last_mut() {
                side.insert(word);
            }
        }
    }
    let side = |words: &BTreeSet<&str>| words.iter().copied().collect::<Vec<_>>().join(" ");
    let mut out = side(&sides[0]);
    for (direction, words) in directions.iter().zip(&sides[1..]) {
        out.push_str(&format!(" | {direction} | {}", side(words)));
    }
    out
}

/// Whether the phrase points at what is on screen right now ("close this",
/// "click that"). Such a request means something only at the moment it was
/// said, so a routine learned from it would replay against whatever is on
/// screen the next time.
#[must_use]
pub fn is_deictic(phrase: &str) -> bool {
    phrase
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| w == "this" || w == "that")
}

/// The routine `phrase` asks for among `routines`, if any. A phrase with no
/// words of its own (all filler) matches nothing.
#[must_use]
pub fn find<'a>(routines: &'a [Routine], phrase: &str) -> Option<&'a Routine> {
    if word_set(phrase).is_empty() {
        return None;
    }
    let want = key(phrase);
    routines.iter().find(|r| key(&r.phrase) == want)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routine(phrase: &str) -> Routine {
        Routine {
            phrase: phrase.to_string(),
            steps: Vec::new(),
            date: String::new(),
            last_used_ms: 0,
            taught: false,
        }
    }

    #[test]
    fn the_same_words_in_any_order_are_one_request() {
        assert_eq!(key("open the notes app"), key("Notes app, open it please!"));
        assert_eq!(key("put notes left"), key("left: put NOTES"));
    }

    #[test]
    fn filler_words_are_ignored_and_the_rest_are_not() {
        assert_eq!(
            word_set("could you please open the notes")
                .into_iter()
                .collect::<Vec<_>>(),
            ["notes", "open"]
        );
        assert_ne!(key("open notes"), key("open notes and mail"));
    }

    #[test]
    fn direction_words_keep_opposite_requests_apart() {
        assert_ne!(key("turn the volume up"), key("turn the volume down"));
        assert_ne!(key("turn wifi on"), key("turn wifi off"));
        assert_ne!(key("turn wifi on"), key("turn wifi"));
    }

    #[test]
    fn digits_and_other_scripts_are_words() {
        assert_eq!(key("open tab 2"), key("tab 2 open"));
        assert_ne!(key("open tab 2"), key("open tab 3"));
        assert!(!word_set("खोलो नोट्स").is_empty());
    }

    #[test]
    fn this_and_that_are_deictic_whatever_the_case_and_punctuation() {
        assert!(is_deictic("close this window"));
        assert!(is_deictic("Click THAT."));
        assert!(is_deictic("save (this)"));
        assert!(!is_deictic("close the window"));
        // A word that merely contains it is not the word.
        assert!(!is_deictic("open thatcher notes"));
        assert!(!is_deictic("open thisisfine"));
    }

    #[test]
    fn find_matches_by_word_set_and_never_by_a_near_miss() {
        let routines = [routine("open notes"), routine("send the weekly report")];
        assert_eq!(
            find(&routines, "Notes, open please").map(|r| r.phrase.as_str()),
            Some("open notes")
        );
        assert!(find(&routines, "open notes and mail").is_none());
        assert!(find(&routines, "open").is_none());
        assert!(find(&routines, "report weekly send").is_some());
    }

    #[test]
    fn the_words_on_each_side_of_a_direction_word_stay_on_that_side() {
        assert_ne!(
            key("copy backup files to desktop"),
            key("copy desktop files to backup")
        );
        // Any order still holds within a side.
        assert_eq!(
            key("copy backup files to desktop"),
            key("files backup copy, to the desktop")
        );
        let routines = [routine("copy backup files to desktop")];
        assert!(find(&routines, "copy desktop files to backup").is_none());
        assert!(find(&routines, "desktop to copy files backup").is_none());
        assert!(find(&routines, "copy files backup to desktop").is_some());
        // The same words under another direction word are another request.
        assert_ne!(key("move notes to trash"), key("move notes from trash"));
        // Two direction words keep three sides.
        assert_ne!(key("copy a from b to c"), key("copy a from c to b"));
        assert_eq!(key("copy a from b to c"), key("a copy from b to c"));
    }

    #[test]
    fn a_phrase_of_nothing_but_filler_matches_nothing() {
        let routines = [routine("")];
        assert!(word_set("please do the").is_empty());
        assert!(find(&routines, "please do the").is_none());
        assert!(find(&routines, "").is_none());
    }
}
