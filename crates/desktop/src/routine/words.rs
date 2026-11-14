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

/// The phrase's identity: its word set, sorted and joined. Two phrases with the
/// same key are the same request.
#[must_use]
pub fn key(phrase: &str) -> String {
    word_set(phrase).into_iter().collect::<Vec<_>>().join(" ")
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
    let want = word_set(phrase);
    if want.is_empty() {
        return None;
    }
    routines.iter().find(|r| word_set(&r.phrase) == want)
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
    fn a_phrase_of_nothing_but_filler_matches_nothing() {
        let routines = [routine("")];
        assert!(word_set("please do the").is_empty());
        assert!(find(&routines, "please do the").is_none());
        assert!(find(&routines, "").is_none());
    }
}
