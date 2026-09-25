//! Keys for the Custom order: strings that sort where a person put a file.
//!
//! Custom sorts every file by one string, ascending. A file nobody arranged
//! sorts by [`default_key`] — its sort date, then its path. A file somebody
//! placed carries a key of its own in its sidecar, generated here to sit
//! between two existing keys, so placing a file is one write and nothing else
//! is renumbered.
//!
//! Pure: this module knows strings and one SQL fragment, and nothing about
//! sidecars, requests or which file is which.
//!
//! **Keys compare by bytes.** That is SQLite's default `BINARY` collation and
//! Rust's `str` ordering, so what these functions promise is what the
//! statement does.
//!
//! **Generated characters are printable ASCII without `"` and `\`**, so a
//! sidecar needs no escapes and stays readable with `grep`. The range has to
//! reach below `0`: a default key ends in a path, and a neighbour whose path
//! continues the anchor's with a space or a `-` (`a.jpg`, `a.jpg - copy.jpg`)
//! leaves room only for characters that sort before `0`. A generated suffix
//! never ends in the lowest character, a space, so every generated key has room
//! below it.

/// Why no key could be produced: the bounds are equal, or the only strings
/// between them need a character outside the alphabet — a control character
/// in a path, right where the anchor's path ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoRoom;

/// The offset that keeps every sort date positive, so a fixed-width decimal
/// orders the way the number does. 2^62 seconds either side of 1970.
///
/// **A durable format**, with [`default_key`]: keys stored in sidecars were
/// generated relative to default keys, so changing this, the width, or the
/// order of the two parts moves every arranged file relative to every
/// unarranged one, in every gallery, with no error.
pub const DEFAULT_KEY_OFFSET: i64 = 1 << 62;

/// A file's key when nobody has placed it: newest first, then by path.
///
/// `alias` is the `media_meta` alias in the statement it lands in. The date is
/// the same coalesced value [`crate::sort::sorter::SORT_DATE`] orders by, so an
/// unarranged gallery under Custom reads as Date does.
pub fn default_key(alias: &str) -> String {
    format!(
        "printf('%019d', {DEFAULT_KEY_OFFSET} - COALESCE({alias}.date_taken, {alias}.mtime)) || {alias}.path"
    )
}

/// [`default_key`] computed in Rust, for the planners' tests and for pinning
/// the SQL against it. `sort_date` is epoch seconds.
pub fn default_key_of(sort_date: i64, path: &str) -> String {
    format!("{:019}{path}", DEFAULT_KEY_OFFSET - sort_date)
}

const LOW: u8 = b' ';
const MID: u8 = b'V';

fn in_alphabet(b: u8) -> bool {
    (LOW..=b'~').contains(&b) && b != b'"' && b != b'\\'
}

/// The shortest non-empty alphabet string strictly below `r`, not ending in
/// [`LOW`].
fn below(r: &[u8]) -> Option<String> {
    let (&c, rest) = r.split_first()?;
    if MID < c {
        return Some((MID as char).to_string());
    }
    let between: Vec<u8> = (LOW + 1..c.min(b'~' + 1)).filter(|&b| in_alphabet(b)).collect();
    if let Some(&m) = between.get(between.len() / 2) {
        return Some((m as char).to_string());
    }
    if c > LOW {
        // Nothing fits strictly between, but a string starting with the lowest
        // character is below `c` whatever follows it.
        return Some(format!("{}{}", LOW as char, MID as char));
    }
    if c == LOW {
        return below(rest).map(|t| format!("{}{t}", LOW as char));
    }
    None
}

/// A key just after `a` and below `b`: the shortest `a + suffix` that fits.
///
/// **It hugs `a`.** Nothing can sort between a file and the file it was placed
/// behind unless its own key extends the anchor's — which a default key does
/// only for a file of the same second whose path extends the anchor's path. A
/// key halfway between the two neighbours would instead land wherever the
/// midpoint of their dates falls, and the next import from the same event
/// would slot in between.
pub fn after(a: &str, b: Option<&str>) -> Result<String, NoRoom> {
    let Some(b) = b else {
        return Ok(format!("{a}{}", MID as char));
    };
    if b <= a {
        return Err(NoRoom);
    }
    match b.as_bytes().strip_prefix(a.as_bytes()) {
        Some(rest) => below(rest).map(|s| format!("{a}{s}")).ok_or(NoRoom),
        // `a < b` and `b` does not extend `a`, so they differ inside `a` and
        // anything appended to `a` stays below `b`.
        None => Ok(format!("{a}{}", MID as char)),
    }
}

/// A key just before `b` and above `a`, hugging `b` from below.
///
/// `b`'s last character is replaced by one below it, so the result shares
/// everything but that last position with `b`. Applied again to its own result
/// it steps further down the same position, and grows by a character only when
/// that position runs out — about once in seven. It never shrinks, which
/// matters for the top drop: there `b` is the first file's key and the shared
/// prefix is its date, and a key that shrank on every drop would eat into the
/// date digits and start sorting above files newer than its anchor.
pub fn before(b: &str, a: Option<&str>) -> Result<String, NoRoom> {
    if a.is_some_and(|a| b <= a) {
        return Err(NoRoom);
    }
    let candidate = b.char_indices().last().map(|(i, ch)| {
        let head = &b[..i];
        let mut buf = [0u8; 4];
        match below(ch.encode_utf8(&mut buf).as_bytes()) {
            Some(s) => format!("{head}{s}"),
            // The last character is a space or a control character: nothing in
            // the alphabet is below it, but the prefix itself is below `b`.
            None => head.to_string(),
        }
    });
    match (candidate, a) {
        (Some(k), Some(a)) if !k.is_empty() && k.as_str() > a => Ok(k),
        (Some(k), None) if !k.is_empty() => Ok(k),
        (_, Some(a)) => after(a, Some(b)),
        (_, None) => below(b.as_bytes()).ok_or(NoRoom),
    }
}

const DIGITS: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// `n` keys after `a` (or from nothing), evenly spaced and fixed-width.
///
/// Used where a whole run is numbered at once — locking a set, appending to
/// one, concatenating two. Generating them one `after` at a time would give
/// the two-hundredth member a key two hundred characters long.
pub fn spread(a: Option<&str>, n: usize) -> Vec<String> {
    if n == 0 {
        return Vec::new();
    }
    let slots = n as u128 + 1;
    let mut width = 1u32;
    while 62u128.pow(width) < slots {
        width += 1;
    }
    let step = 62u128.pow(width) / slots;
    let prefix = a.unwrap_or("");
    (1..=n as u128)
        .map(|i| {
            let mut v = i * step;
            let mut digits = vec![b'0'; width as usize];
            for d in digits.iter_mut().rev() {
                *d = DIGITS[(v % 62) as usize];
                v /= 62;
            }
            format!("{prefix}{}", String::from_utf8(digits).expect("ASCII digits"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, SeedableRng};

    fn assert_key(k: &str) {
        assert!(!k.is_empty(), "an empty key has nothing below it");
        assert!(!k.ends_with(' '), "a generated key ends in the lowest character: {k:?}");
    }

    fn random_key(rng: &mut impl Rng) -> String {
        // Default keys and generated ones, mixed: digits, then a path with
        // spaces, dashes, dots and a non-ASCII character or two.
        let pool: Vec<char> = "0123456789 -._/abcXYZé日".chars().collect();
        let len = rng.gen_range(1..12);
        (0..len).map(|_| pool[rng.gen_range(0..pool.len())]).collect()
    }

    #[test]
    fn after_lands_strictly_between_and_hugs_its_anchor() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(7);
        for _ in 0..5000 {
            let (x, y) = (random_key(&mut rng), random_key(&mut rng));
            if x == y {
                continue;
            }
            let (a, b) = if x < y { (x, y) } else { (y, x) };
            match after(&a, Some(&b)) {
                Ok(k) => {
                    assert!(a < k && k < b, "{a:?} < {k:?} < {b:?}");
                    assert!(k.starts_with(&a), "{k:?} does not extend its anchor {a:?}");
                    assert_key(&k);
                }
                // Only when `b` continues `a` with a character below the
                // alphabet — a control character, or the space alone at the end.
                Err(NoRoom) => {
                    let rest = &b.as_bytes()[a.len()..];
                    assert!(b.starts_with(&a) && rest.iter().all(|&c| c <= LOW), "{a:?} {b:?}");
                }
            }
        }
    }

    #[test]
    fn before_lands_strictly_between() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(11);
        for _ in 0..5000 {
            let (x, y) = (random_key(&mut rng), random_key(&mut rng));
            if x == y {
                continue;
            }
            let (a, b) = if x < y { (x, y) } else { (y, x) };
            if let Ok(k) = before(&b, Some(&a)) {
                assert!(a < k && k < b, "{a:?} < {k:?} < {b:?}");
            }
            let k = before(&b, None).unwrap();
            assert!(k < b, "{k:?} < {b:?}");
        }
    }

    #[test]
    fn equal_bounds_have_no_room() {
        assert_eq!(after("abc", Some("abc")), Err(NoRoom));
        assert_eq!(before("abc", Some("abc")), Err(NoRoom));
    }

    #[test]
    fn a_control_character_where_the_anchor_ends_has_no_room() {
        assert_eq!(after("a", Some("a\u{1}b")), Err(NoRoom));
    }

    #[test]
    fn a_path_continuing_with_a_space_or_dash_still_has_room() {
        for b in ["a.jpg - copy.jpg", "a.jpg copy.jpg", "a.jpg!"] {
            let k = after("a.jpg", Some(b)).unwrap();
            assert!("a.jpg" < k.as_str() && k.as_str() < b, "{k:?} for {b:?}");
        }
    }

    #[test]
    fn placing_again_behind_the_same_anchor_keeps_fitting() {
        // Each placement behind `a` goes directly after it, ahead of the one
        // before — the keys must keep fitting, and grow only slowly.
        let a = default_key_of(1_700_000_000, "photos/a.jpg");
        let mut upper: Option<String> = None;
        for _ in 0..200 {
            let k = after(&a, upper.as_deref()).unwrap();
            assert!(k > a);
            if let Some(u) = &upper {
                assert!(&k < u);
            }
            upper = Some(k);
        }
        assert!(upper.unwrap().len() < a.len() + 40);
    }

    #[test]
    fn a_later_import_never_lands_between_a_file_and_its_anchor() {
        let anchor = default_key_of(1_700_000_000, "cam1/IMG_0001.jpg");
        let next = default_key_of(1_600_000_000, "cam1/IMG_0002.jpg");
        let placed = after(&anchor, Some(&next)).unwrap();
        // The second camera's photos from the same event, a second either side
        // and at the same second.
        for (date, path) in [
            (1_700_000_001, "cam2/DSC_0001.jpg"),
            (1_699_999_999, "cam2/DSC_0002.jpg"),
            (1_700_000_000, "cam2/DSC_0003.jpg"),
            (1_700_000_000, "cam0/DSC_0004.jpg"),
        ] {
            let k = default_key_of(date, path);
            assert!(
                !(anchor < k && k < placed),
                "{path} landed between the anchor and the file placed behind it"
            );
        }
    }

    #[test]
    fn a_top_drop_stays_below_later_arrivals_however_often_it_is_repeated() {
        // The user's decision: later arrivals land above a photo dropped at the
        // top. Each drop anchors to the current first key, which after the
        // first drop is the previous drop's.
        let first = default_key_of(1_700_000_000, "photos/a.jpg");
        let mut top = first.clone();
        for _ in 0..500 {
            let k = before(&top, None).unwrap();
            assert!(k < top);
            top = k;
        }
        assert!(top.starts_with(&first[..19]), "the date digits were eaten: {top:?}");
        assert!(top.len() < first.len() + 100, "grew too fast: {} chars", top.len());
        let later = default_key_of(1_700_000_001, "photos/z.jpg");
        assert!(later < top, "a later arrival must sort above the top drop");
    }

    #[test]
    fn spread_is_increasing_fixed_width_and_extends_its_anchor() {
        for n in [1, 2, 61, 62, 200, 5000] {
            let keys = spread(Some("pos"), n);
            assert_eq!(keys.len(), n);
            assert!(keys.windows(2).all(|w| w[0] < w[1]), "n = {n}");
            assert!(keys.iter().all(|k| k.starts_with("pos") && k.len() == keys[0].len()));
            assert!(keys.iter().all(|k| k.as_str() > "pos"));
        }
        assert!(spread(None, 0).is_empty());
        assert_eq!(spread(None, 200)[0].len(), 2);
    }

    #[test]
    fn the_default_key_in_sql_is_the_default_key_in_rust() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE media_meta (path TEXT, date_taken INTEGER, mtime INTEGER NOT NULL);",
        )
        .unwrap();
        let rows: [(&str, Option<i64>, i64); 4] = [
            ("photos/a.jpg", Some(1_700_000_000), 5),
            ("scans/1969.tif", Some(-2_208_988_800), 5),
            ("downloads/9f3e.png", None, 1_650_000_000),
            ("日本/写真.jpg", None, 0),
        ];
        for (path, taken, mtime) in rows {
            conn.execute(
                "INSERT INTO media_meta VALUES (?1, ?2, ?3)",
                rusqlite::params![path, taken, mtime],
            )
            .unwrap();
            let sql = format!("SELECT {} FROM media_meta m WHERE m.path = ?1", default_key("m"));
            let got: String = conn.query_row(&sql, [path], |r| r.get(0)).unwrap();
            assert_eq!(got, default_key_of(taken.unwrap_or(mtime), path));
        }
        // Pinned: the format is durable, so the literal is the test.
        assert_eq!(
            default_key_of(1_700_000_000, "photos/a.jpg"),
            "4611686016727387904photos/a.jpg"
        );
        assert_eq!(default_key_of(-2_208_988_800, "x"), "4611686020636376704x");
    }

    #[test]
    fn newer_sorts_first() {
        assert!(default_key_of(1_700_000_001, "z") < default_key_of(1_700_000_000, "a"));
    }
}
