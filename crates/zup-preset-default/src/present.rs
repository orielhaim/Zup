//! Resolved component groups, decided before anything is drawn.
//!
//! A group is one decision. Every member of a group is presented with that
//! group, and nowhere else. Prominence says how clearly the decision is
//! exposed. Whether the defaults are enough to install is a separate question.

// `InstallOptions` is only named by the tests in this module, which build one
// by hand to present; the resolution below reads it through `Surface`.
#[cfg(test)]
use zup_preset_sdk::host::InstallOptions;
use zup_preset_sdk::host::{ComponentGroupOption, ComponentProminence, SelectionRequirement};
use zup_preset_sdk::prelude::*;

use crate::model::{self, ComponentRow};

/// A primary group larger than this is a summary that opens the full set,
/// rather than a list. The set is still the whole group.
const INLINE_LIMIT: usize = 6;

/// Where a resolved group sits. `Auto` has already been decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    Primary,
    Secondary,
}

/// One group, with every member it contains.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedGroup {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub placement: Placement,
    pub selection: SelectionRequirement,
    pub rows: Vec<ComponentRow>,
}

impl ResolvedGroup {
    /// A short reading of how much of the group is selected.
    pub fn count(&self) -> String {
        let selected = self.rows.iter().filter(|row| row_selected(row)).count();
        format!("{selected} of {} selected", self.rows.len())
    }

    /// Names of a few selected components, for a large group's summary.
    pub fn selected_names(&self) -> String {
        let names: Vec<&str> = self
            .rows
            .iter()
            .filter(|row| row_selected(row))
            .map(|row| row.name.as_str())
            .collect();
        match names.as_slice() {
            [] => "Nothing selected".into(),
            [one] => (*one).to_owned(),
            [first, second] => format!("{first}, {second}"),
            [first, second, third, rest @ ..] => {
                format!("{first}, {second}, {third} + {} more", rest.len())
            }
        }
    }

    /// Whether this group, if it demands an explicit choice, has one.
    pub fn satisfied(&self) -> bool {
        self.selection == SelectionRequirement::Defaulted
            || self
                .rows
                .iter()
                .any(|row| matches!(row.kind, model::ComponentKind::Optional { selected: true }))
    }

    /// A primary group small enough to list in place.
    pub fn inline(&self) -> bool {
        self.placement != Placement::Primary || self.rows.len() <= INLINE_LIMIT
    }
}

/// The install surface's component decisions, plus the choices that are not
/// components.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallSurface {
    pub primary: Vec<ResolvedGroup>,
    pub secondary: Vec<ResolvedGroup>,
    pub show_location: bool,
    pub show_scope: bool,
}

impl InstallSurface {
    pub fn of(snapshot: &Snapshot) -> Self {
        let resolved = resolve(snapshot);
        Self {
            primary: resolved
                .iter()
                .filter(|group| group.placement == Placement::Primary)
                .cloned()
                .collect(),
            secondary: resolved
                .iter()
                .filter(|group| group.placement == Placement::Secondary)
                .cloned()
                .collect(),
            show_location: snapshot.surface.allows_directory_override(),
            show_scope: model::scope_choices(snapshot).len() >= 2,
        }
    }

    /// Why Install cannot proceed, when a group still needs an explicit choice.
    pub fn blocked(&self) -> Option<String> {
        self.primary
            .iter()
            .chain(&self.secondary)
            .find(|group| !group.satisfied())
            .map(|group| format!("Choose at least one item in {}", group.title))
    }

    /// What the collapsed customization row should say.
    pub fn customize_summary(&self, scope: InstallScope) -> Option<String> {
        let groups = &self.secondary;
        if groups.is_empty() {
            return self.show_scope.then(|| model::audience(scope).to_owned());
        }
        match groups.as_slice() {
            [one] => Some(match one.title.as_str() {
                "Components" => one.count().replace(" selected", " components"),
                _ => format!("{} · {}", one.title, one.count()),
            }),
            _ => Some(
                groups
                    .iter()
                    .map(|group| format!("{} · {}", group.title, group.count()))
                    .collect::<Vec<_>>()
                    .join(" · "),
            ),
        }
    }
}

/// Every group that should be shown, each with all of its members.
pub fn resolve(snapshot: &Snapshot) -> Vec<ResolvedGroup> {
    let rows = model::component_rows(&snapshot.surface);
    let by_id: std::collections::BTreeMap<&ComponentId, &ComponentRow> =
        rows.iter().map(|row| (&row.id, row)).collect();
    let mut claimed = std::collections::BTreeSet::new();
    let mut resolved = Vec::new();
    for group in snapshot.surface.groups() {
        let members: Vec<ComponentRow> = group
            .components
            .iter()
            .filter_map(|id| by_id.get(id).map(|row| (*row).clone()))
            .collect();
        for id in &group.components {
            claimed.insert(id);
        }
        if let Some(resolved_group) = resolve_one(group, members) {
            resolved.push(resolved_group);
        }
    }
    let rest: Vec<ComponentRow> = rows
        .into_iter()
        .filter(|row| !claimed.contains(&row.id))
        .collect();
    if let Some(group) = resolve_one(
        &ComponentGroupOption {
            id: String::new(),
            label: None,
            description: None,
            prominence: ComponentProminence::Auto,
            selection: SelectionRequirement::Defaulted,
            components: Vec::new(),
        },
        rest,
    ) {
        resolved.insert(0, group);
    }
    resolved
}

fn resolve_one(group: &ComponentGroupOption, rows: Vec<ComponentRow>) -> Option<ResolvedGroup> {
    if rows.is_empty() {
        return None;
    }
    let optional = rows
        .iter()
        .any(|row| matches!(row.kind, model::ComponentKind::Optional { .. }));
    let placement = match group.prominence {
        ComponentProminence::Primary => Placement::Primary,
        ComponentProminence::Secondary => Placement::Secondary,
        ComponentProminence::Auto => {
            if !optional {
                return None;
            }
            if group.selection == SelectionRequirement::Explicit {
                Placement::Primary
            } else {
                Placement::Secondary
            }
        }
    };
    let title = group
        .label
        .clone()
        .filter(|label| !label.is_empty())
        .unwrap_or_else(|| "Components".into());
    Some(ResolvedGroup {
        id: group.id.clone(),
        title,
        description: group.description.clone(),
        placement,
        selection: group.selection,
        rows,
    })
}

fn row_selected(row: &ComponentRow) -> bool {
    match row.kind {
        model::ComponentKind::Required => true,
        model::ComponentKind::Optional { selected } => selected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn component(id: &str, required: bool, selected: bool) -> ComponentOption {
        ComponentOption {
            id: ComponentId::new(id).expect("id"),
            name: id.into(),
            description: None,
            required,
            selected: selected || required,
            installed: false,
        }
    }

    fn group(
        id: &str,
        label: Option<&str>,
        prominence: ComponentProminence,
        selection: SelectionRequirement,
        ids: &[&str],
    ) -> ComponentGroupOption {
        ComponentGroupOption {
            id: id.into(),
            label: label.map(str::to_owned),
            description: None,
            prominence,
            selection,
            components: ids
                .iter()
                .map(|id| ComponentId::new(id).expect("id"))
                .collect(),
        }
    }

    fn surface(components: Vec<ComponentOption>, groups: Vec<ComponentGroupOption>) -> Snapshot {
        Snapshot {
            product: ProductIdentity {
                name: "Demo".into(),
                publisher: None,
                version: "1.0.0".into(),
                description: None,
            },
            surface: Surface::Install(InstallOptions {
                existing_version: None,
                scopes: vec![InstallScope::User],
                scope: InstallScope::User,
                components,
                groups,
                install_directory: None,
                allow_directory_override: false,
            }),
            state: InstallerState::Options,
            operation: None,
            progress: None,
            plan: PlanStatus::Unsupported,
            diagnostic: None,
            update: None,
            repair_drift: Vec::new(),
            launch: None,
        }
    }

    fn ids(group: &ResolvedGroup) -> Vec<&str> {
        group.rows.iter().map(|row| row.id.as_str()).collect()
    }

    #[test]
    fn a_package_with_nothing_optional_has_no_component_decision() {
        let snapshot = surface(vec![component("app", true, true)], Vec::new());
        assert!(resolve(&snapshot).is_empty());
    }

    #[test]
    fn an_ordinary_group_stays_together_under_customize() {
        let snapshot = surface(
            vec![
                component("app", true, true),
                component("docs", false, false),
                component("samples", false, true),
            ],
            Vec::new(),
        );
        let resolved = resolve(&snapshot);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].placement, Placement::Secondary);
        assert_eq!(ids(&resolved[0]), vec!["app", "docs", "samples"]);
    }

    #[test]
    fn a_small_primary_group_lists_every_member() {
        let snapshot = surface(
            vec![
                component("app", true, true),
                component("cli", false, true),
                component("ide", false, false),
            ],
            vec![group(
                "main",
                Some("Choose what to install"),
                ComponentProminence::Primary,
                SelectionRequirement::Defaulted,
                &["app", "cli", "ide"],
            )],
        );
        let surface = InstallSurface::of(&snapshot);
        assert_eq!(surface.primary.len(), 1);
        assert!(surface.secondary.is_empty());
        assert!(surface.primary[0].inline());
        assert_eq!(ids(&surface.primary[0]), vec!["app", "cli", "ide"]);
    }

    #[test]
    fn a_large_primary_group_is_still_one_complete_set() {
        let components: Vec<_> = (0..12)
            .map(|index| component(&format!("part{index}"), index == 0, index < 3))
            .collect();
        let ids_all: Vec<_> = (0..12).map(|index| format!("part{index}")).collect();
        let refs: Vec<&str> = ids_all.iter().map(String::as_str).collect();
        let snapshot = surface(
            components,
            vec![group(
                "all",
                Some("Components"),
                ComponentProminence::Primary,
                SelectionRequirement::Defaulted,
                &refs,
            )],
        );
        let resolved = resolve(&snapshot);
        assert_eq!(resolved.len(), 1);
        assert!(!resolved[0].inline());
        assert_eq!(resolved[0].rows.len(), 12);
    }

    #[test]
    fn two_groups_stay_separate_and_each_stays_whole() {
        let snapshot = surface(
            vec![
                component("web", false, true),
                component("desktop", false, false),
                component("docs", false, true),
                component("samples", false, false),
            ],
            vec![
                group(
                    "work",
                    Some("Workloads"),
                    ComponentProminence::Primary,
                    SelectionRequirement::Defaulted,
                    &["web", "desktop"],
                ),
                group(
                    "extras",
                    Some("Optional tools"),
                    ComponentProminence::Secondary,
                    SelectionRequirement::Defaulted,
                    &["docs", "samples"],
                ),
            ],
        );
        let surface = InstallSurface::of(&snapshot);
        assert_eq!(ids(&surface.primary[0]), vec!["web", "desktop"]);
        assert_eq!(ids(&surface.secondary[0]), vec!["docs", "samples"]);
        assert!(
            surface
                .customize_summary(InstallScope::User)
                .unwrap()
                .contains("Optional tools")
        );
    }

    #[test]
    fn an_explicit_group_blocks_install_until_an_optional_item_is_chosen() {
        let snapshot = surface(
            vec![
                component("web", false, false),
                component("desktop", false, false),
            ],
            vec![group(
                "work",
                Some("Workloads"),
                ComponentProminence::Primary,
                SelectionRequirement::Explicit,
                &["web", "desktop"],
            )],
        );
        let surface = InstallSurface::of(&snapshot);
        assert!(surface.blocked().is_some());
        assert_eq!(surface.primary[0].placement, Placement::Primary);
    }
}
