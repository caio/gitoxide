use git_testtools::once_cell::sync::Lazy;

static BASELINE: Lazy<baseline::Baseline> = Lazy::new(|| baseline::parse().unwrap());

pub mod baseline {
    use crate::matching::BASELINE;
    use bstr::{BStr, BString, ByteSlice, ByteVec};
    use git_hash::ObjectId;
    use git_refspec::parse::Operation;
    use git_refspec::MatchGroup;
    use git_testtools::once_cell::sync::Lazy;
    use std::borrow::Borrow;
    use std::collections::HashMap;

    #[derive(Debug)]
    pub struct Ref {
        pub name: BString,
        pub target: ObjectId,
        /// Set if this is a tag, pointing to the tag object itself
        pub tag: Option<ObjectId>,
    }

    impl Ref {
        pub fn to_item(&self) -> git_refspec::match_group::Item<'_> {
            git_refspec::match_group::Item {
                full_ref_name: self.name.borrow(),
                target: &self.target,
                tag: self.tag.as_deref(),
            }
        }
    }

    static INPUT: Lazy<Vec<Ref>> = Lazy::new(|| parse_input().unwrap());

    pub type Baseline = HashMap<Vec<BString>, Result<Vec<Mapping>, BString>>;

    #[derive(Debug)]
    pub struct Mapping {
        pub remote: BString,
        /// `None` if there is no destination/tracking branch
        pub local: Option<BString>,
    }

    pub fn input() -> impl Iterator<Item = git_refspec::match_group::Item<'static>> + ExactSizeIterator + Clone {
        INPUT.iter().map(Ref::to_item)
    }

    pub fn provides_does_not_actually_match_object_names<'a, 'b>(
        specs: impl IntoIterator<Item = &'a str> + Clone,
        expected: impl IntoIterator<Item = &'b str>,
    ) {
        check_fetch_remote(
            specs,
            Mode::ObjectHashSource {
                expected: expected.into_iter().map(|s| s.into()).collect(),
            },
        )
    }

    pub fn agrees_with_fetch_specs<'a>(specs: impl IntoIterator<Item = &'a str> + Clone) {
        check_fetch_remote(specs, Mode::Normal)
    }

    enum Mode<'a> {
        Normal,
        ObjectHashSource { expected: Vec<&'a BStr> },
    }

    fn check_fetch_remote<'a>(specs: impl IntoIterator<Item = &'a str> + Clone, mode: Mode) {
        let match_group = MatchGroup::from_fetch_specs(
            specs
                .clone()
                .into_iter()
                .map(|spec| git_refspec::parse(spec.into(), Operation::Fetch).unwrap()),
        );

        let key: Vec<_> = specs.into_iter().map(BString::from).collect();
        let expected = BASELINE
            .get(&key)
            .unwrap_or_else(|| panic!("BUG: Need {:?} added to the baseline", key))
            .as_ref()
            .expect("no error");

        let actual = match_group.match_remotes(input());
        assert_eq!(
            actual.len(),
            match &mode {
                Mode::Normal => expected.len(),
                Mode::ObjectHashSource { expected } => expected.len(),
            },
            "got a different amount of mappings: {:?} != {:?}",
            actual,
            expected
        );

        for (idx, (actual, expected)) in actual.iter().zip(expected).enumerate() {
            match &mode {
                Mode::ObjectHashSource { .. } => {}
                Mode::Normal => {
                    assert_eq!(actual.lhs, &expected.remote, "{}: remote mismatch", idx);
                }
            }
            if let Some(expected) = expected.local.as_ref() {
                match actual.rhs.as_ref() {
                    None => panic!("{}: Expected local ref to be {}, got none", idx, expected),
                    Some(actual) => assert_eq!(actual.as_ref(), expected, "{}: mismatched local ref", idx),
                }
            }
        }

        if let Mode::ObjectHashSource { expected } = &mode {
            assert_eq!(
                actual.iter().map(|a| a.lhs).collect::<Vec<_>>(),
                *expected,
                "object hash expectations should be aligned in code"
            );
        }
    }

    fn parse_input() -> crate::Result<Vec<Ref>> {
        let dir = git_testtools::scripted_fixture_repo_read_only("match_baseline.sh")?;
        let refs_buf = std::fs::read(dir.join("clone").join("remote-refs.list"))?;
        let mut out = Vec::new();
        for line in refs_buf.lines() {
            if line.starts_with(b"From ") {
                continue;
            }
            let mut tokens = line.splitn(2, |b| *b == b'\t');
            let target = ObjectId::from_hex(tokens.next().expect("hex-sha"))?;
            let name = tokens.next().expect("name");
            if !name.ends_with(b"^{}") {
                out.push(Ref {
                    name: name.into(),
                    target,
                    tag: None,
                })
            } else {
                let last = out.last_mut().unwrap();
                let tag = last.target;
                last.target = target;
                last.tag = Some(tag);
            }
        }
        Ok(out)
    }

    pub(crate) fn parse() -> crate::Result<Baseline> {
        let dir = git_testtools::scripted_fixture_repo_read_only("match_baseline.sh")?;
        let buf = std::fs::read(dir.join("clone").join("baseline.git"))?;

        let mut map = HashMap::new();
        let mut mappings = Vec::new();
        let mut fatal = None;
        for line in buf.lines() {
            if line.starts_with(b"From ") {
                continue;
            }
            match line.strip_prefix(b"specs: ") {
                Some(specs) => {
                    let key: Vec<_> = specs.split(|b| *b == b' ').map(BString::from).collect();
                    let value = match fatal.take() {
                        Some(message) => Err(message),
                        None => Ok(std::mem::take(&mut mappings)),
                    };
                    map.insert(key, value);
                }
                None => match line.strip_prefix(b"fatal: ") {
                    Some(message) => {
                        fatal = Some(message.into());
                    }
                    None => {
                        let past_note = line
                            .splitn(2, |b| *b == b']')
                            .nth(1)
                            .or_else(|| line.strip_prefix(b" * branch "))
                            .or_else(|| line.strip_prefix(b" * tag "))
                            .unwrap_or_else(|| panic!("line unhandled: {:?}", line.as_bstr()));
                        let mut tokens = past_note.split(|b| *b == b' ').filter(|t| !t.is_empty());

                        let lhs = tokens.next().unwrap().trim();
                        tokens.next();
                        let rhs = tokens.next().unwrap().trim();
                        mappings.push(Mapping {
                            remote: full_remote_ref(lhs.into()),
                            local: (rhs != b"FETCH_HEAD").then(|| full_tracking_ref(rhs.into())),
                        })
                    }
                },
            }
        }

        Ok(map)
    }

    fn looks_like_tag(name: &BString) -> bool {
        name.starts_with(b"v0.")
    }

    fn full_remote_ref(mut name: BString) -> BString {
        if !name.contains(&b'/') {
            if looks_like_tag(&name) {
                name.insert_str(0, b"refs/tags/");
            } else if let Ok(_id) = git_hash::ObjectId::from_hex(name.as_ref()) {
                // keep as is
            } else {
                name.insert_str(0, b"refs/heads/");
            }
        }
        name
    }

    fn full_tracking_ref(mut name: BString) -> BString {
        if name.starts_with_str(b"origin/") {
            name.insert_str(0, b"refs/remotes/");
        } else if looks_like_tag(&name) {
            name.insert_str(0, b"refs/tags/");
        }
        name
    }
}
