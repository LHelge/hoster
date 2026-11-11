//! The updates a stack is offered (ADR-0061 §5, §9): what the update checker stored, kept to
//! the active revision and gathered into update groups.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use crate::stack::{Digest, ImageRef, Stack};
use crate::store::{DigestSet, ServiceUpdates, Timestamp, UpdateKind};

/// One update the UI offers: a moved tag of one service, or a newer version for an update
/// group (services whose images share registry, namespace and tag), whose members move
/// together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateOffer {
    pub kind: UpdateKind,
    /// The services it updates, in name order; more than one for a group.
    pub services: Vec<String>,
    /// The tag the services are on.
    pub from: String,
    /// The tag they move to; the same tag for a [`UpdateKind::Digest`] update.
    pub to: String,
    /// The digest a moved tag points at now.
    pub digest: Option<Digest>,
    /// When the update was first found (the earliest member's, for a group).
    pub first_seen: Timestamp,
}

impl UpdateOffer {
    /// The choice that selects this offer in a form: its first service and its kind.
    pub fn choice(&self) -> UpdateChoice {
        UpdateChoice {
            service: self.services[0].clone(),
            kind: self.kind,
        }
    }
}

/// A choice of update in a form: `service:kind`, e.g. `server:minor`. For a group, any member
/// selects the whole group.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct UpdateChoice {
    pub service: String,
    pub kind: UpdateKind,
}

impl FromStr for UpdateChoice {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (service, kind) = s
            .rsplit_once(':')
            .ok_or_else(|| format!("invalid update choice {s:?}: expected `service:kind`"))?;
        Ok(Self {
            service: service.to_owned(),
            kind: kind.parse()?,
        })
    }
}

impl fmt::Display for UpdateChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.service, self.kind)
    }
}

/// The updates on offer for one stack's active revision.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StackUpdates {
    /// By first service, then kind.
    pub offers: Vec<UpdateOffer>,
    /// Services whose last check failed, with why.
    pub errors: Vec<(String, String)>,
    /// When the stack's services were last checked; `None` before the first check.
    pub checked_at: Option<Timestamp>,
}

impl StackUpdates {
    /// The offers for `definition` (the active revision, which pins `pins`) from the checker's
    /// `rows` of its stack. A row checked for another image, tag or digest is stale and left
    /// out until the next check.
    pub fn new(definition: &Stack, pins: &DigestSet, rows: Vec<ServiceUpdates>) -> Self {
        let checkable: BTreeMap<&str, &ImageRef> = definition
            .services
            .iter()
            .filter(|(_, s)| !s.image.is_ci_built() && s.image.tag().is_some())
            .map(|(name, s)| (name.as_ref(), &s.image))
            .collect();
        let mut fresh: BTreeMap<String, ServiceUpdates> = BTreeMap::new();
        let mut updates = Self::default();
        for row in rows {
            let Some(image) = checkable.get(row.service.as_str()) else {
                continue;
            };
            let current = format!("{}/{}", image.registry(), image.repository());
            if row.image != current
                || Some(row.tag.as_str()) != image.tag()
                || pins.get(&row.service) != Some(&row.digest)
            {
                continue;
            }
            updates.checked_at = updates.checked_at.max(Some(row.checked_at));
            if let Some(error) = &row.error {
                updates.errors.push((row.service.clone(), error.clone()));
            }
            fresh.insert(row.service.clone(), row);
        }

        // Moved tags, one service each.
        for row in fresh.values() {
            for stored in &row.candidates {
                if stored.candidate.kind == UpdateKind::Digest {
                    updates.offers.push(UpdateOffer {
                        kind: UpdateKind::Digest,
                        services: vec![row.service.clone()],
                        from: row.tag.clone(),
                        to: row.tag.clone(),
                        digest: stored.candidate.digest.clone(),
                        first_seen: stored.first_seen,
                    });
                }
            }
        }

        // Versions, per group: offered only when every member has the same target.
        let mut groups: BTreeMap<(&str, &str, &str), Vec<&str>> = BTreeMap::new();
        for (service, image) in &checkable {
            let namespace = image
                .repository()
                .rsplit_once('/')
                .map_or("", |(namespace, _)| namespace);
            let tag = image.tag().unwrap_or_default();
            groups
                .entry((image.registry(), namespace, tag))
                .or_default()
                .push(service);
        }
        for ((_, _, tag), members) in &groups {
            for kind in [UpdateKind::Patch, UpdateKind::Minor, UpdateKind::Major] {
                let found: Option<Vec<_>> = members
                    .iter()
                    .map(|member| {
                        fresh.get(*member).and_then(|row| {
                            row.candidates.iter().find(|c| c.candidate.kind == kind)
                        })
                    })
                    .collect();
                let Some(found) = found else { continue };
                let to = &found[0].candidate.tag;
                if found.iter().any(|c| &c.candidate.tag != to) {
                    continue;
                }
                updates.offers.push(UpdateOffer {
                    kind,
                    services: members.iter().map(|m| (*m).to_owned()).collect(),
                    from: (*tag).to_owned(),
                    to: to.clone(),
                    digest: None,
                    first_seen: found
                        .iter()
                        .map(|c| c.first_seen)
                        .min()
                        .expect("a group has members"),
                });
            }
        }
        updates
            .offers
            .sort_by(|a, b| (&a.services[0], a.kind).cmp(&(&b.services[0], b.kind)));
        updates
    }

    /// The offers "Update all" applies: everything but majors.
    pub fn applicable(&self) -> impl Iterator<Item = &UpdateOffer> {
        self.offers.iter().filter(|o| o.kind != UpdateKind::Major)
    }

    /// The offer for each service that `choices` update, by service. A group's choice covers
    /// all its members, and a service chosen twice takes the larger update (a version over a
    /// moved tag, a minor over a patch). A choice of something not on offer is returned as
    /// the error.
    pub fn chosen<'a>(
        &'a self,
        choices: &[UpdateChoice],
    ) -> Result<BTreeMap<&'a str, &'a UpdateOffer>, UpdateChoice> {
        let mut chosen: BTreeMap<&str, &UpdateOffer> = BTreeMap::new();
        for choice in choices {
            let offer = self
                .offers
                .iter()
                .find(|o| o.kind == choice.kind && o.services.contains(&choice.service))
                .ok_or_else(|| choice.clone())?;
            for service in &offer.services {
                let slot = chosen.entry(service.as_str()).or_insert(offer);
                if offer.kind > slot.kind {
                    *slot = offer;
                }
            }
        }
        Ok(chosen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Candidate, StoredCandidate};

    const STACK: &str = r#"
version = 1
name = "photos"

[services.server]
image = "ghcr.io/immich-app/immich-server:v2.3.1"

[services.ml]
image = "ghcr.io/immich-app/immich-machine-learning:v2.3.1"

[services.db]
image = "docker.io/library/postgres:16"

[services.app]
image = "ghcr.io/acme/app"
"#;

    fn digest(n: u8) -> String {
        format!("sha256:{}", format!("{n:02x}").repeat(32))
    }

    fn pins() -> DigestSet {
        ["server", "ml", "db", "app"]
            .into_iter()
            .map(|s| (s.to_owned(), digest(1)))
            .collect()
    }

    fn row(
        service: &str,
        image: &str,
        tag: &str,
        candidates: &[(UpdateKind, &str)],
    ) -> ServiceUpdates {
        ServiceUpdates {
            stack: "photos".into(),
            service: service.into(),
            image: image.into(),
            tag: tag.into(),
            digest: digest(1),
            checked_at: Timestamp::now(),
            error: None,
            candidates: candidates
                .iter()
                .map(|(kind, tag)| StoredCandidate {
                    candidate: Candidate {
                        kind: *kind,
                        tag: (*tag).into(),
                        digest: (*kind == UpdateKind::Digest).then(|| digest(2).parse().unwrap()),
                    },
                    first_seen: Timestamp::now(),
                })
                .collect(),
        }
    }

    fn server(candidates: &[(UpdateKind, &str)]) -> ServiceUpdates {
        row(
            "server",
            "ghcr.io/immich-app/immich-server",
            "v2.3.1",
            candidates,
        )
    }

    fn ml(candidates: &[(UpdateKind, &str)]) -> ServiceUpdates {
        row(
            "ml",
            "ghcr.io/immich-app/immich-machine-learning",
            "v2.3.1",
            candidates,
        )
    }

    fn summary(updates: &StackUpdates) -> Vec<(String, UpdateKind, String)> {
        updates
            .offers
            .iter()
            .map(|o| (o.services.join("+"), o.kind, o.to.clone()))
            .collect()
    }

    #[test]
    fn groups_offer_a_version_only_when_every_member_has_it() {
        let stack: Stack = STACK.parse().unwrap();
        let updates = StackUpdates::new(
            &stack,
            &pins(),
            vec![
                server(&[(UpdateKind::Patch, "v2.3.4"), (UpdateKind::Minor, "v2.4.1")]),
                // ml has no v2.4.1 yet: the minor waits; the patch both have goes.
                ml(&[(UpdateKind::Patch, "v2.3.4"), (UpdateKind::Minor, "v2.4.0")]),
                row(
                    "db",
                    "docker.io/library/postgres",
                    "16",
                    &[(UpdateKind::Digest, "16"), (UpdateKind::Major, "17")],
                ),
            ],
        );
        assert_eq!(
            summary(&updates),
            [
                ("db".into(), UpdateKind::Digest, "16".into()),
                ("db".into(), UpdateKind::Major, "17".into()),
                ("ml+server".into(), UpdateKind::Patch, "v2.3.4".into()),
            ]
        );
        let applicable: Vec<_> = updates.applicable().map(|o| o.kind).collect();
        assert_eq!(applicable, [UpdateKind::Digest, UpdateKind::Patch]);
        assert!(updates.checked_at.is_some());
    }

    #[test]
    fn stale_rows_are_left_out() {
        let stack: Stack = STACK.parse().unwrap();
        let mut moved_on = pins();
        moved_on.insert("db".into(), digest(7));
        let updates = StackUpdates::new(
            &stack,
            &moved_on,
            vec![
                // Checked for v2.3.0; the revision is on v2.3.1 now.
                row(
                    "server",
                    "ghcr.io/immich-app/immich-server",
                    "v2.3.0",
                    &[(UpdateKind::Patch, "v2.3.4")],
                ),
                ml(&[(UpdateKind::Patch, "v2.3.4")]),
                // Checked against another digest.
                row(
                    "db",
                    "docker.io/library/postgres",
                    "16",
                    &[(UpdateKind::Digest, "16")],
                ),
                // A service the revision no longer has.
                row(
                    "gone",
                    "docker.io/library/redis",
                    "7",
                    &[(UpdateKind::Digest, "7")],
                ),
            ],
        );
        // server's fresh row is missing, so the group's patch is not offered either.
        assert!(updates.offers.is_empty(), "{:?}", updates.offers);
    }

    #[test]
    fn choices_expand_to_groups_and_take_the_larger_update() {
        let stack: Stack = STACK.parse().unwrap();
        let mut db = row(
            "db",
            "docker.io/library/postgres",
            "16",
            &[(UpdateKind::Digest, "16")],
        );
        db.error = Some("docker.io is rate-limiting requests".into());
        let updates = StackUpdates::new(
            &stack,
            &pins(),
            vec![
                server(&[
                    (UpdateKind::Digest, "v2.3.1"),
                    (UpdateKind::Minor, "v2.4.0"),
                ]),
                ml(&[(UpdateKind::Minor, "v2.4.0")]),
                db,
            ],
        );
        assert_eq!(
            updates.errors,
            [(
                "db".to_owned(),
                "docker.io is rate-limiting requests".to_owned()
            )]
        );
        let choices: Vec<UpdateChoice> = ["server:digest", "ml:minor"]
            .into_iter()
            .map(|c| c.parse().unwrap())
            .collect();
        let chosen = updates.chosen(&choices).unwrap();
        let kinds: Vec<(&str, UpdateKind)> = chosen.iter().map(|(s, o)| (*s, o.kind)).collect();
        assert_eq!(
            kinds,
            [("ml", UpdateKind::Minor), ("server", UpdateKind::Minor)]
        );

        let missing = updates.chosen(&["db:major".parse().unwrap()]).unwrap_err();
        assert_eq!(missing.to_string(), "db:major");
        assert!("db".parse::<UpdateChoice>().is_err());
        assert!("db:huge".parse::<UpdateChoice>().is_err());
    }
}
