use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use nix::mount::{MsFlags, mount};
use nix::unistd::chdir;

use crate::error::{Error, IoContext, NixContext, Result};

pub const LOWERDIR: &str = "dev.mars.overlay.lowerdir";
pub const UPPERDIR: &str = "dev.mars.overlay.upperdir";
pub const WORKDIR: &str = "dev.mars.overlay.workdir";

pub const MAX_OPTION_BYTES: usize = 4096;
pub const MIN_READONLY_LOWER: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layers {
    pub lower: Vec<PathBuf>,
    pub upper: Option<PathBuf>,
    pub work: Option<PathBuf>,
}

impl Layers {
    pub fn is_readonly(&self) -> bool {
        self.upper.is_none()
    }
}

pub fn from_annotations(
    annotations: &HashMap<String, String>,
    bundle: &Path,
) -> Result<Option<Layers>> {
    let lower = annotations.get(LOWERDIR).map(String::as_str);
    let upper = annotations.get(UPPERDIR).map(String::as_str);
    let work = annotations.get(WORKDIR).map(String::as_str);

    let Some(lower) = lower.filter(|value| !value.trim().is_empty()) else {
        if upper.is_some() || work.is_some() {
            return Err(Error::Overlay(format!(
                "{UPPERDIR} or {WORKDIR} is set but {LOWERDIR} is not"
            )));
        }
        return Ok(None);
    };

    let lower: Vec<PathBuf> = split_lowerdir(lower)
        .into_iter()
        .map(|dir| absolute(bundle, &dir))
        .collect();

    if lower.is_empty() {
        return Err(Error::Overlay(format!("{LOWERDIR} lists no directories")));
    }

    Ok(Some(Layers {
        lower,
        upper: upper.map(|dir| absolute(bundle, dir)),
        work: work.map(|dir| absolute(bundle, dir)),
    }))
}

fn absolute(bundle: &Path, dir: &str) -> PathBuf {
    let path = PathBuf::from(dir);

    if path.is_absolute() {
        path
    } else {
        bundle.join(path)
    }
}

pub fn split_lowerdir(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut chars = value.chars();

    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(escaped) = chars.next() {
                    current.push(escaped);
                }
            }
            ':' => out.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }

    out.push(current);
    out.into_iter().filter(|part| !part.is_empty()).collect()
}

pub fn escape(path: &Path) -> Result<String> {
    let text = path.to_string_lossy();

    if text.contains(',') {
        return Err(Error::Overlay(format!(
            "{text:?} contains a comma; overlayfs separates mount options with commas and \
             provides no way to escape one"
        )));
    }

    Ok(text.replace('\\', "\\\\").replace(':', "\\:"))
}

pub fn options(layers: &Layers) -> Result<String> {
    let lower = layers
        .lower
        .iter()
        .map(|dir| escape(dir))
        .collect::<Result<Vec<_>>>()?;

    let mut parts = vec![format!("lowerdir={}", lower.join(":"))];

    if let Some(upper) = &layers.upper {
        parts.push(format!("upperdir={}", escape(upper)?));
    }
    if let Some(work) = &layers.work {
        parts.push(format!("workdir={}", escape(work)?));
    }

    Ok(parts.join(","))
}

pub fn prepare(layers: &Layers) -> Result<()> {
    for dir in &layers.lower {
        if !dir.is_dir() {
            return Err(Error::Overlay(format!(
                "lower layer {} does not exist or is not a directory",
                dir.display()
            )));
        }
    }

    let (upper, work) = match (&layers.upper, &layers.work) {
        (Some(upper), Some(work)) => (upper, work),
        (None, None) if layers.lower.len() < MIN_READONLY_LOWER => {
            return Err(Error::Overlay(format!(
                "a read-only overlay needs at least {MIN_READONLY_LOWER} lower layers, {} given; \
                 with one layer and no upperdir there is nothing to merge, and the kernel refuses \
                 the mount",
                layers.lower.len()
            )));
        }
        (Some(_), None) => {
            return Err(Error::Overlay(format!(
                "{UPPERDIR} requires {WORKDIR}: overlayfs needs a scratch directory on the same \
                 filesystem as the upper layer to stage rename and copy-up operations"
            )));
        }
        (None, Some(_)) => {
            return Err(Error::Overlay(format!(
                "{WORKDIR} was set without {UPPERDIR}; a read-only overlay uses neither"
            )));
        }
        (None, None) => return Ok(()),
    };

    fs::create_dir_all(upper).ctx(format!("create upperdir {}", upper.display()))?;
    fs::create_dir_all(work).ctx(format!("create workdir {}", work.display()))?;

    let upper = fs::canonicalize(upper).ctx(format!("resolve upperdir {}", upper.display()))?;
    let work = fs::canonicalize(work).ctx(format!("resolve workdir {}", work.display()))?;

    check_same_filesystem(&upper, &work)?;
    check_not_nested(&upper, &work)?;

    for dir in &layers.lower {
        let lower = fs::canonicalize(dir).ctx(format!("resolve lower layer {}", dir.display()))?;

        if lower == upper || upper.starts_with(&lower) || lower.starts_with(&upper) {
            return Err(Error::Overlay(format!(
                "lower layer {} overlaps the upper layer {}; overlayfs requires them to be \
                 disjoint directory trees",
                lower.display(),
                upper.display()
            )));
        }
    }

    Ok(())
}

fn check_same_filesystem(upper: &Path, work: &Path) -> Result<()> {
    let upper_dev = fs::metadata(upper)
        .ctx(format!("stat upperdir {}", upper.display()))?
        .dev();
    let work_dev = fs::metadata(work)
        .ctx(format!("stat workdir {}", work.display()))?
        .dev();

    if upper_dev != work_dev {
        return Err(Error::Overlay(format!(
            "workdir {} is on device {work_dev} but upperdir {} is on device {upper_dev}; \
             overlayfs renames files between the two and cannot cross a filesystem boundary",
            work.display(),
            upper.display(),
        )));
    }

    Ok(())
}

fn check_not_nested(upper: &Path, work: &Path) -> Result<()> {
    if work.starts_with(upper) {
        return Err(Error::Overlay(format!(
            "workdir {} is inside upperdir {}; the container would see the runtime's scratch \
             directory as part of its own filesystem",
            work.display(),
            upper.display(),
        )));
    }

    if upper.starts_with(work) {
        return Err(Error::Overlay(format!(
            "upperdir {} is inside workdir {}; overlayfs empties the workdir on mount",
            upper.display(),
            work.display(),
        )));
    }

    Ok(())
}

pub fn common_parent(layers: &Layers) -> Option<PathBuf> {
    let mut all: Vec<&Path> = layers.lower.iter().map(PathBuf::as_path).collect();
    all.extend(layers.upper.as_deref());
    all.extend(layers.work.as_deref());

    let mut shared: Vec<_> = all.first()?.parent()?.components().collect();

    for path in &all[1..] {
        let candidate: Vec<_> = path.parent()?.components().collect();
        let keep = shared
            .iter()
            .zip(candidate.iter())
            .take_while(|(a, b)| a == b)
            .count();
        shared.truncate(keep);
    }

    let joined: PathBuf = shared.iter().collect();

    if joined.is_absolute() {
        Some(joined)
    } else {
        None
    }
}

pub fn relative_to(layers: &Layers, base: &Path) -> Option<Layers> {
    let strip = |path: &Path| -> Option<PathBuf> {
        path.strip_prefix(base)
            .ok()
            .filter(|rest| !rest.as_os_str().is_empty())
            .map(Path::to_path_buf)
    };

    Some(Layers {
        lower: layers
            .lower
            .iter()
            .map(|dir| strip(dir))
            .collect::<Option<Vec<_>>>()?,
        upper: match &layers.upper {
            Some(dir) => Some(strip(dir)?),
            None => None,
        },
        work: match &layers.work {
            Some(dir) => Some(strip(dir)?),
            None => None,
        },
    })
}

pub fn mount_at(target: &Path, layers: &Layers) -> Result<()> {
    let absolute = options(layers)?;

    let (data, base) = if absolute.len() <= MAX_OPTION_BYTES {
        (absolute, None)
    } else {
        let base = common_parent(layers).ok_or_else(|| too_long(absolute.len(), None))?;
        let relative =
            relative_to(layers, &base).ok_or_else(|| too_long(absolute.len(), Some(&base)))?;
        let shortened = options(&relative)?;

        if shortened.len() > MAX_OPTION_BYTES {
            return Err(too_long(shortened.len(), Some(&base)));
        }

        (shortened, Some(base))
    };

    if let Some(base) = &base {
        tracing::debug!(
            base = %base.display(),
            bytes = data.len(),
            "overlay option string exceeded one page, using paths relative to a common parent"
        );
        chdir(base.as_path()).ctx(format!("chdir to overlay base {}", base.display()))?;
    }

    let result = mount(
        Some("overlay"),
        target,
        Some("overlay"),
        MsFlags::empty(),
        Some(data.as_str()),
    )
    .ctx(format!(
        "mount overlay at {} with {} lower layer(s), {} (options {:?}) — overlayfs reports the \
         reason for a refusal only in the kernel log, so check `dmesg | tail`",
        target.display(),
        layers.lower.len(),
        if layers.is_readonly() {
            "read-only"
        } else {
            "writable"
        },
        data,
    ));

    if base.is_some() {
        chdir("/").ctx("chdir back to / after mounting the overlay")?;
    }

    result
}

fn too_long(bytes: usize, base: Option<&Path>) -> Error {
    let hint = match base {
        Some(base) => format!(
            " even relative to {}; shorten the layer directory names",
            base.display()
        ),
        None => " and the layers share no common parent to make them relative to".to_string(),
    };

    Error::Overlay(format!(
        "the mount option string is {bytes} bytes, over the {MAX_OPTION_BYTES}-byte page the \
         kernel accepts{hint}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layers(lower: &[&str], upper: Option<&str>, work: Option<&str>) -> Layers {
        Layers {
            lower: lower.iter().map(PathBuf::from).collect(),
            upper: upper.map(PathBuf::from),
            work: work.map(PathBuf::from),
        }
    }

    #[test]
    fn lowerdir_is_colon_separated_and_ordered_top_layer_first() {
        assert_eq!(split_lowerdir("/a:/b:/c"), vec!["/a", "/b", "/c"]);
    }

    #[test]
    fn a_colon_in_a_layer_name_can_be_escaped() {
        assert_eq!(split_lowerdir("/a\\:1:/b"), vec!["/a:1", "/b"]);
        assert_eq!(split_lowerdir("/a\\\\b:/c"), vec!["/a\\b", "/c"]);
    }

    #[test]
    fn empty_segments_are_dropped() {
        assert_eq!(split_lowerdir("/a::/b:"), vec!["/a", "/b"]);
    }

    #[test]
    fn escaping_round_trips_through_the_splitter() {
        for original in ["/plain", "/with:colon", "/with\\backslash", "/both:\\x"] {
            let escaped = escape(Path::new(original)).unwrap();
            assert_eq!(split_lowerdir(&escaped), vec![original.to_string()]);
        }
    }

    #[test]
    fn a_comma_in_a_layer_name_is_rejected_rather_than_silently_truncated() {
        let error = escape(Path::new("/layers/a,b")).unwrap_err();
        assert!(matches!(error, Error::Overlay(message) if message.contains("comma")));
    }

    #[test]
    fn read_only_overlay_omits_upper_and_work() {
        let data = options(&layers(&["/l1", "/l2"], None, None)).unwrap();
        assert_eq!(data, "lowerdir=/l1:/l2");
        assert!(layers(&["/l1"], None, None).is_readonly());
    }

    #[test]
    fn a_read_only_overlay_of_one_layer_is_rejected_before_the_kernel_sees_it() {
        let error = prepare(&layers(&["/nonexistent-single"], None, None)).unwrap_err();
        assert!(matches!(error, Error::Overlay(m) if m.contains("does not exist")));

        let error = prepare(&layers(&["/tmp"], None, None)).unwrap_err();
        assert!(matches!(error, Error::Overlay(m) if m.contains("at least 2 lower layers")));
    }

    #[test]
    fn writable_overlay_names_all_three_directories() {
        let data = options(&layers(&["/l1"], Some("/u"), Some("/w"))).unwrap();
        assert_eq!(data, "lowerdir=/l1,upperdir=/u,workdir=/w");
    }

    #[test]
    fn nested_work_and_upper_are_rejected_in_both_directions() {
        let error =
            check_not_nested(Path::new("/x/upper"), Path::new("/x/upper/work")).unwrap_err();
        assert!(matches!(error, Error::Overlay(m) if m.contains("inside upperdir")));

        let error = check_not_nested(Path::new("/x/work/upper"), Path::new("/x/work")).unwrap_err();
        assert!(matches!(error, Error::Overlay(m) if m.contains("inside workdir")));

        check_not_nested(Path::new("/x/upper"), Path::new("/x/work")).unwrap();
    }

    #[test]
    fn common_parent_is_the_deepest_shared_directory() {
        let found = common_parent(&layers(
            &["/var/lib/mars/l1", "/var/lib/mars/l2"],
            Some("/var/lib/mars/upper"),
            Some("/var/lib/mars/work"),
        ));
        assert_eq!(found, Some(PathBuf::from("/var/lib/mars")));

        let found = common_parent(&layers(&["/var/lib/a/l1", "/var/lib/b/l2"], None, None));
        assert_eq!(found, Some(PathBuf::from("/var/lib")));
    }

    #[test]
    fn relative_layers_reproduce_the_absolute_ones_when_rejoined() {
        let original = layers(
            &["/var/lib/mars/l1", "/var/lib/mars/l2"],
            Some("/var/lib/mars/upper"),
            Some("/var/lib/mars/work"),
        );
        let base = common_parent(&original).unwrap();
        let relative = relative_to(&original, &base).unwrap();

        assert_eq!(
            options(&relative).unwrap(),
            "lowerdir=l1:l2,upperdir=upper,workdir=work"
        );
        assert!(options(&relative).unwrap().len() < options(&original).unwrap().len());

        for (short, long) in relative.lower.iter().zip(original.lower.iter()) {
            assert_eq!(&base.join(short), long);
        }
    }

    #[test]
    fn annotations_without_overlay_keys_mean_no_overlay() {
        let none = from_annotations(&HashMap::new(), Path::new("/bundle")).unwrap();
        assert!(none.is_none());
    }

    #[test]
    fn upper_without_lower_is_a_configuration_error() {
        let mut annotations = HashMap::new();
        annotations.insert(UPPERDIR.to_string(), "/u".to_string());

        let error = from_annotations(&annotations, Path::new("/bundle")).unwrap_err();
        assert!(matches!(error, Error::Overlay(m) if m.contains(LOWERDIR)));
    }

    #[test]
    fn relative_layer_paths_resolve_against_the_bundle() {
        let mut annotations = HashMap::new();
        annotations.insert(
            LOWERDIR.to_string(),
            "layers/base:/absolute/one".to_string(),
        );
        annotations.insert(UPPERDIR.to_string(), "diff".to_string());
        annotations.insert(WORKDIR.to_string(), "work".to_string());

        let found = from_annotations(&annotations, Path::new("/bundle"))
            .unwrap()
            .unwrap();

        assert_eq!(
            found.lower,
            vec![
                PathBuf::from("/bundle/layers/base"),
                PathBuf::from("/absolute/one"),
            ]
        );
        assert_eq!(found.upper, Some(PathBuf::from("/bundle/diff")));
        assert_eq!(found.work, Some(PathBuf::from("/bundle/work")));
    }
}
