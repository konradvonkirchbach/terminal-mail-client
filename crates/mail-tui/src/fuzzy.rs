//! A minimal subsequence-based fuzzy matcher for compose's recipient
//! autocomplete. Hand-rolled rather than pulling in a matching crate — in
//! the same spirit as the widgets in `editable.rs`, this stays small
//! enough to own directly.

use mail_core::Address;

/// Scores how well `needle` fuzzy-matches `haystack`, case-insensitively.
/// `None` means some character of `needle` never showed up, in order, in
/// `haystack` — an empty `needle` always returns `None` too, since "match
/// everything" isn't a useful suggestion signal. Higher scores are better
/// matches: an earlier first match and longer contiguous runs both score
/// more, so a prefix or exact substring consistently outscores a
/// scattered subsequence match.
pub fn score(needle: &str, haystack: &str) -> Option<i32> {
    if needle.is_empty() {
        return None;
    }

    let haystack_lower = haystack.to_lowercase();
    let needle_lower = needle.to_lowercase();
    let hay: Vec<char> = haystack_lower.chars().collect();
    let needle: Vec<char> = needle_lower.chars().collect();

    let mut total = 0i32;
    let mut hay_idx = 0;
    let mut consecutive = 0i32;
    let mut first_match = None;

    for &nc in &needle {
        let matched_at = loop {
            if hay_idx >= hay.len() {
                break None;
            }
            if hay[hay_idx] == nc {
                break Some(hay_idx);
            }
            consecutive = 0;
            hay_idx += 1;
        };
        let idx = matched_at?;
        first_match.get_or_insert(idx);
        consecutive += 1;
        total += 1 + consecutive; // contiguous runs score increasingly more
        hay_idx = idx + 1;
    }

    // Reward an earlier first match — prefix matches rank highest.
    total += 20 - (first_match.unwrap_or(0) as i32).min(20);
    Some(total)
}

/// The text a sender is fuzzy-matched and displayed against: "Name
/// email" when a display name is known, otherwise just the email.
fn label(address: &Address) -> String {
    match &address.name {
        Some(name) if !name.trim().is_empty() => format!("{name} {}", address.email),
        _ => address.email.clone(),
    }
}

/// The best `limit` senders matching `query`, highest score first.
/// Non-matches are dropped entirely rather than scored at the bottom.
pub fn best_matches(query: &str, senders: &[Address], limit: usize) -> Vec<Address> {
    let mut scored: Vec<(i32, &Address)> = senders
        .iter()
        .filter_map(|a| score(query, &label(a)).map(|s| (s, a)))
        .collect();
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    scored.into_iter().take(limit).map(|(_, a)| a.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(name: &str, email: &str) -> Address {
        Address { name: Some(name.to_string()), email: email.to_string() }
    }

    #[test]
    fn score_requires_every_needle_character_to_appear_in_order() {
        assert!(score("ac", "abc").is_some());
        assert!(score("ca", "abc").is_none(), "'c' then 'a' never appears in that order in \"abc\"");
    }

    #[test]
    fn score_is_case_insensitive() {
        assert!(score("ALICE", "alice@example.com").is_some());
    }

    #[test]
    fn score_returns_none_for_an_empty_needle() {
        assert_eq!(score("", "anything"), None);
    }

    #[test]
    fn score_rewards_an_earlier_match_over_a_later_one() {
        let earlier = score("ali", "alice@example.com").unwrap();
        let later = score("ali", "somealice@example.com").unwrap();
        assert!(earlier > later);
    }

    #[test]
    fn score_rewards_a_contiguous_run_over_a_scattered_match() {
        let contiguous = score("al", "alice").unwrap();
        let scattered = score("al", "axlice").unwrap();
        assert!(contiguous > scattered);
    }

    #[test]
    fn best_matches_finds_a_name_match_and_excludes_non_matches() {
        let senders = vec![
            addr("Alice Doe", "alice@example.com"),
            addr("Bob Smith", "bob@example.com"),
            addr("Carol", "carol@example.com"),
        ];
        let results = best_matches("ali", &senders, 8);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].email, "alice@example.com");
    }

    #[test]
    fn best_matches_matches_against_the_email_too_not_just_the_name() {
        let senders = vec![addr("Whoever", "quirky@example.com")];
        let results = best_matches("quirky", &senders, 8);
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn best_matches_respects_the_limit() {
        let senders: Vec<Address> = (0..20).map(|i| addr("A", &format!("a{i}@example.com"))).collect();
        assert_eq!(best_matches("a", &senders, 5).len(), 5);
    }

    #[test]
    fn best_matches_returns_nothing_for_a_query_with_no_match() {
        let senders = vec![addr("Zzz", "zzz@example.com")];
        assert!(best_matches("qqq", &senders, 8).is_empty());
    }

    #[test]
    fn best_matches_ranks_a_better_match_first() {
        let senders = vec![
            addr("Somewhere Alice", "somewherealice@example.com"),
            addr("Alice", "alice@example.com"),
        ];
        let results = best_matches("alice", &senders, 8);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].email, "alice@example.com", "the prefix/earlier match must rank first");
    }
}
