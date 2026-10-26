//! Telling accounts apart in a few letters.

use crate::Pool;

/// Which account a pool is, when its provider has more than one: the
/// first word of its label, e.g. "work" or "personal". When another
/// account's label starts the same, as `sam.lee@gmail.com` and
/// `sam@northwindtraders.com` do, the first word that tells it apart,
/// trying an email's domain first: "gmail". None when it is its
/// provider's only account.
pub fn account_short(pool: &Pool, pools: &[Pool]) -> Option<String> {
    let others: Vec<Vec<String>> = (pools.iter())
        .filter(|other| other.provider == pool.provider && other.id != pool.id)
        .map(|other| {
            let (local, domain) = label_words(&other.account_label);
            [local, domain].concat()
        })
        .collect();
    if others.is_empty() {
        return None;
    }
    let (local, domain) = label_words(&pool.account_label);
    let first = (local.first().or(domain.first()))
        .cloned()
        .unwrap_or_else(|| pool.account_label.to_lowercase());
    let word = if others.iter().any(|other| other.first() == Some(&first)) {
        (domain.iter().chain(&local))
            .find(|word| others.iter().all(|other| !other.contains(word)))
            .cloned()
            .unwrap_or(first)
    } else {
        first
    };
    Some(word.chars().take(10).collect())
}

/// A label's words, lowercased: an email's local part, and its domain
/// without the top level ("gmail" of gmail.com). Other labels are all
/// local part.
pub fn label_words(label: &str) -> (Vec<String>, Vec<String>) {
    let split = |part: &str| -> Vec<String> {
        part.split(['-', '.', ' ', '_'])
            .filter(|word| !word.is_empty())
            .map(str::to_lowercase)
            .collect()
    };
    match label.split_once('@') {
        Some((local, domain)) => {
            let mut domain = split(domain);
            domain.pop();
            (split(local), domain)
        }
        None => (split(label), Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(id: &str, provider: &str, label: &str) -> Pool {
        Pool {
            id: id.into(),
            provider: provider.into(),
            account_label: label.into(),
            status: "ok".into(),
            windows: Vec::new(),
            ..Default::default()
        }
    }

    #[test]
    fn accounts_are_named_by_what_sets_them_apart() {
        let pools = [
            pool("a", "claude", "sam.lee@gmail.com"),
            pool("b", "claude", "sam@northwindtraders.com"),
            pool("c", "codex", "sam@example.com"),
            pool("d", "codex", "work-account"),
        ];
        let short = |i: usize| account_short(&pools[i], &pools);
        assert_eq!(short(0).as_deref(), Some("gmail"));
        assert_eq!(short(1).as_deref(), Some("northwindt"), "cut to ten");
        assert_eq!(short(2).as_deref(), Some("sam"), "no clash, first word");
        assert_eq!(short(3).as_deref(), Some("work"));
        let alone = [pool("e", "gemini", "me@example.com")];
        assert_eq!(account_short(&alone[0], &alone), None);
    }
}
