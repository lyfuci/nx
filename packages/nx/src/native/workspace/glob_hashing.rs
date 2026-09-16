use std::ops::Range;
use std::path::{Path, PathBuf};

use rayon::prelude::*;
use xxhash_rust::xxh3;

use crate::native::glob::build_glob_set;
use crate::native::utils::Normalize;

pub(super) type FileEntry = (PathBuf, String);

/// Globs that resolve without a scan: `<dir>/**/*` is a contiguous slice of
/// the path-sorted table, a plain path is one binary search.
enum Lookup<'a> {
    Prefix(&'a str),
    Literal(&'a str),
}

const GLOB_SPECIAL: &[char] = &['*', '?', '[', ']', '{', '}', '(', ')', '!', '|', '\\'];

fn classify(glob: &str) -> Option<Lookup<'_>> {
    let plain = |s: &str| !s.is_empty() && !s.contains(GLOB_SPECIAL);
    if glob == "**" || glob == "**/*" {
        return Some(Lookup::Prefix(""));
    }
    if let Some(dir) = glob
        .strip_suffix("/**/*")
        .or_else(|| glob.strip_suffix("/**"))
        .or_else(|| glob.strip_suffix('/'))
    {
        return plain(dir).then_some(Lookup::Prefix(dir));
    }
    plain(glob).then_some(Lookup::Literal(glob))
}

fn prefix_range(files: &[FileEntry], dir: &str) -> Range<usize> {
    if dir.is_empty() {
        return 0..files.len();
    }
    let dir = Path::new(dir);
    let start = files.partition_point(|(path, _)| path.as_path() < dir);
    let len = files[start..].partition_point(|(path, _)| path.starts_with(dir));
    start..start + len
}

fn literal_range(files: &[FileEntry], file: &str) -> Option<Range<usize>> {
    let file = Path::new(file);
    files
        .binary_search_by(|(path, _)| path.as_path().cmp(file))
        .ok()
        .map(|i| i..i + 1)
}

/// The union of the group's matches as ordered, non-overlapping index ranges,
/// or `None` when a glob needs the scanning path.
fn lookup_ranges(files: &[FileEntry], globs: &[String]) -> Option<Vec<Range<usize>>> {
    let mut ranges = Vec::with_capacity(globs.len());
    for glob in globs {
        let range = match classify(glob)? {
            Lookup::Prefix(dir) => prefix_range(files, dir),
            Lookup::Literal(file) => match literal_range(files, file) {
                Some(range) => range,
                None => continue,
            },
        };
        if !range.is_empty() {
            ranges.push(range);
        }
    }
    ranges.sort_by_key(|range| range.start);

    let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    Some(merged)
}

fn hash_entry(hasher: &mut xxh3::Xxh3, path: &Path, hash: &str) {
    match path.to_str() {
        Some(file) if !cfg!(windows) => hasher.update(file.as_bytes()),
        _ => hasher.update(path.to_normalized_string().as_bytes()),
    }
    hasher.update(hash.as_bytes());
}

fn hash_by_scan(files: &[FileEntry], globs: &[String]) -> anyhow::Result<String> {
    let glob_set = build_glob_set(globs)?;
    let mut hasher = xxh3::Xxh3::new();
    for (path, hash) in files {
        let file = path.to_normalized_string();
        if glob_set.is_match(&file) {
            hasher.update(file.as_bytes());
            hasher.update(hash.as_bytes());
        }
    }
    Ok(hasher.digest().to_string())
}

fn hash_group(files: &[FileEntry], globs: &[String]) -> anyhow::Result<String> {
    let Some(ranges) = lookup_ranges(files, globs) else {
        return hash_by_scan(files, globs);
    };
    let mut hasher = xxh3::Xxh3::new();
    for range in ranges {
        for (path, hash) in &files[range] {
            hash_entry(&mut hasher, path, hash);
        }
    }
    Ok(hasher.digest().to_string())
}

/// One digest per glob group. `files` must be sorted by path, as the files
/// worker keeps it.
pub(super) fn hash_glob_groups(
    files: &[FileEntry],
    glob_groups: &[Vec<String>],
) -> anyhow::Result<Vec<String>> {
    glob_groups
        .par_iter()
        .map(|globs| hash_group(files, globs))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(paths: &[&str]) -> Vec<FileEntry> {
        let mut files: Vec<FileEntry> = paths
            .iter()
            .map(|p| (PathBuf::from(p), format!("hash-of-{p}")))
            .collect();
        files.sort();
        files
    }

    fn sample() -> Vec<FileEntry> {
        table(&[
            "package.json",
            "pnpm-lock.yaml",
            "tsconfig.base.json",
            "eslint.config.js",
            "libs/a/src/index.ts",
            "libs/a/src/index.spec.ts",
            "libs/a/tsconfig.json",
            "libs/a/.eslintignore",
            "libs/a/nested/src/deep.ts",
            "libs/a/nested/tsconfig.json",
            "libs/a-b/src/index.ts",
            "libs/a.ts",
            "libs/b/src/index.ts",
            "libs/b/eslint.config.js",
            "apps/web/src/main.ts",
            "apps/@scope/pkg/src/main.ts",
        ])
    }

    fn strs(globs: &[&str]) -> Vec<String> {
        globs.iter().map(|g| g.to_string()).collect()
    }

    fn assert_same_as_scan(files: &[FileEntry], globs: &[&str]) {
        let globs = strs(globs);
        assert_eq!(
            hash_group(files, &globs).unwrap(),
            hash_by_scan(files, &globs).unwrap(),
            "globs {globs:?}"
        );
    }

    #[test]
    fn classifies_globs() {
        assert!(matches!(
            classify("libs/a/**/*"),
            Some(Lookup::Prefix("libs/a"))
        ));
        assert!(matches!(
            classify("libs/a/**"),
            Some(Lookup::Prefix("libs/a"))
        ));
        assert!(matches!(
            classify("libs/a/"),
            Some(Lookup::Prefix("libs/a"))
        ));
        assert!(matches!(classify("**/*"), Some(Lookup::Prefix(""))));
        assert!(matches!(classify("**"), Some(Lookup::Prefix(""))));
        assert!(matches!(
            classify("libs/a/.eslintignore"),
            Some(Lookup::Literal("libs/a/.eslintignore"))
        ));
        assert!(matches!(
            classify("apps/@scope/pkg/tsconfig.json"),
            Some(Lookup::Literal("apps/@scope/pkg/tsconfig.json"))
        ));
        assert!(classify("libs/**/*.spec.ts").is_none());
        assert!(classify("libs/{a,b}/**/*").is_none());
        assert!(classify("!libs/a/**/*").is_none());
        assert!(classify("**/*.{ts,js}").is_none());
        assert!(classify("/**/*").is_none());
        assert!(classify("/").is_none());
        assert!(classify("").is_none());
    }

    #[test]
    fn prefix_range_is_the_project_subtree_only() {
        let files = sample();
        let paths: Vec<&str> = files[prefix_range(&files, "libs/a")]
            .iter()
            .map(|(p, _)| p.to_str().unwrap())
            .collect();
        assert_eq!(
            paths,
            [
                "libs/a/.eslintignore",
                "libs/a/nested/src/deep.ts",
                "libs/a/nested/tsconfig.json",
                "libs/a/src/index.spec.ts",
                "libs/a/src/index.ts",
                "libs/a/tsconfig.json",
            ]
        );
        assert_eq!(prefix_range(&files, "libs/a/nested").len(), 2);
        assert_eq!(prefix_range(&files, "libs/missing").len(), 0);
        assert_eq!(prefix_range(&files, ""), 0..files.len());
    }

    #[test]
    fn overlapping_lookups_merge_without_duplicates() {
        let files = sample();
        let ranges = lookup_ranges(
            &files,
            &strs(&[
                "libs/a/**/*",
                "libs/a/tsconfig.json",
                "libs/a/nested/**/*",
                "pnpm-lock.yaml",
                "tsconfig.base.json",
                "libs/a/does-not-exist.json",
            ]),
        )
        .unwrap();
        let total: usize = ranges.iter().map(|r| r.len()).sum();
        assert_eq!(total, 6 + 2);
        for pair in ranges.windows(2) {
            assert!(pair[0].end < pair[1].start);
        }
    }

    #[test]
    fn matches_the_scanning_path() {
        let files = sample();
        assert_same_as_scan(&files, &["libs/a/**/*"]);
        assert_same_as_scan(&files, &["**/*"]);
        assert_same_as_scan(&files, &["libs/a/**/*", "libs/a/.eslintignore"]);
        assert_same_as_scan(
            &files,
            &[
                "libs/a/**/*",
                "eslint.config.js",
                "libs/a/.eslintignore",
                "pnpm-lock.yaml",
                "tsconfig.base.json",
                "libs/a/tsconfig.json",
            ],
        );
        assert_same_as_scan(&files, &["libs/a/nested/**/*", "libs/a/**/*"]);
        assert_same_as_scan(&files, &["apps/@scope/pkg/**/*", "pnpm-lock.yaml"]);
        assert_same_as_scan(&files, &["libs/missing/**/*", "missing.json"]);
        assert_same_as_scan(&files, &["libs/a/"]);
        assert_same_as_scan(&files, &["**/*", "pnpm-lock.yaml"]);
    }

    #[test]
    fn hashes_groups_independently_in_input_order() {
        let files = sample();
        let groups = vec![
            strs(&["libs/a/**/*", "pnpm-lock.yaml"]),
            strs(&["libs/b/**/*", "pnpm-lock.yaml"]),
            strs(&["libs/**/*.spec.ts"]),
            strs(&["libs/a/**/*", "pnpm-lock.yaml"]),
        ];
        let hashes = hash_glob_groups(&files, &groups).unwrap();
        assert_eq!(hashes.len(), 4);
        assert_eq!(hashes[0], hashes[3]);
        assert_ne!(hashes[0], hashes[1]);
        assert_eq!(hashes[2], hash_by_scan(&files, &groups[2]).unwrap());
    }

    #[test]
    fn digest_changes_with_content_and_membership() {
        let files = sample();
        let globs = strs(&["libs/a/**/*", "pnpm-lock.yaml"]);
        let before = hash_group(&files, &globs).unwrap();

        let mut changed = files.clone();
        let (_, hash) = changed
            .iter_mut()
            .find(|(p, _)| p == Path::new("libs/a/src/index.ts"))
            .unwrap();
        *hash = "other".into();
        assert_ne!(before, hash_group(&changed, &globs).unwrap());

        let mut added = files.clone();
        added.push((PathBuf::from("libs/a/src/new.ts"), "n".into()));
        added.sort();
        assert_ne!(before, hash_group(&added, &globs).unwrap());

        let mut unrelated = files.clone();
        unrelated.push((PathBuf::from("libs/b/src/new.ts"), "n".into()));
        unrelated.sort();
        assert_eq!(before, hash_group(&unrelated, &globs).unwrap());
    }
}
