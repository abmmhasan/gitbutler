use anyhow::{anyhow, bail, Context, Result};
use gitbutler_branch::{signature, Branch, BranchId, SignaturePurpose};
use gitbutler_cherry_pick::RepositoryExt as _;
use gitbutler_command_context::CommandContext;
use gitbutler_commit::commit_ext::CommitExt;
use gitbutler_project::access::WorktreeWritePermission;
use gitbutler_repo::{rebase::cherry_rebase_group, LogUntil, RepositoryExt as _};
use serde::{Deserialize, Serialize};

use crate::VirtualBranchesExt as _;

#[derive(Serialize, PartialEq, Debug)]
#[serde(tag = "type", content = "subject")]
pub enum BranchStatus {
    Empty,
    FullyIntegrated,
    Conflicted {
        potentially_conflicted_uncommited_changes: bool,
    },
    SaflyUpdatable,
}

#[derive(Serialize, PartialEq, Debug)]
#[serde(tag = "type", content = "subject")]
pub enum BranchStatuses {
    UpToDate,
    UpdatesRequired(Vec<(BranchId, BranchStatus)>),
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
#[serde(tag = "type", content = "subject")]
enum UpdatableResolutionApproach {
    Rebase,
    Merge,
    Unapply,
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
#[serde(tag = "type", content = "subject")]
enum StatusResolution {
    Empty(UpdatableResolutionApproach),
    Conflicted {
        resolution_approach: UpdatableResolutionApproach,
        potentially_conflicted_uncommited_changes: bool,
    },
    SaflyUpdatable(UpdatableResolutionApproach),
    FullyIntegrated,
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct Resolution {
    branch_id: BranchId,
    resolution: StatusResolution,
}

enum IntegrationResult {
    UpdatedObjects { head: git2::Oid, tree: git2::Oid },
    UnapplyBranch,
    DeleteBranch,
}

impl StatusResolution {
    fn resolution_matches_status(&self, other: &BranchStatus) -> bool {
        match other {
            BranchStatus::Empty => matches!(self, Self::Empty(_)),
            BranchStatus::FullyIntegrated => matches!(self, Self::FullyIntegrated),
            BranchStatus::SaflyUpdatable => matches!(self, Self::SaflyUpdatable(_)),
            BranchStatus::Conflicted {
                potentially_conflicted_uncommited_changes: a,
            } => match self {
                Self::Conflicted {
                    potentially_conflicted_uncommited_changes: b,
                    ..
                } => a == b,
                _ => false,
            },
        }
    }
}

pub struct UpstreamIntegrationContext<'a> {
    _perm: Option<&'a mut WorktreeWritePermission>,
    repository: &'a git2::Repository,
    virtual_branches_in_workspace: Vec<Branch>,
    new_target: git2::Commit<'a>,
    old_target: git2::Commit<'a>,
}

impl<'a> UpstreamIntegrationContext<'a> {
    pub(crate) fn open(
        command_context: &'a CommandContext,
        perm: &'a mut WorktreeWritePermission,
    ) -> Result<Self> {
        let virtual_branches_handle = command_context.project().virtual_branches();
        let target = virtual_branches_handle.get_default_target()?;
        let repository = command_context.repository();
        let target_branch = repository
            .find_branch_by_refname(&target.branch.clone().into())?
            .ok_or(anyhow!("Branch not found"))?;
        let new_target = target_branch.get().peel_to_commit()?;
        let old_target = repository.find_commit(target.sha)?;
        let virtual_branches_in_workspace = virtual_branches_handle.list_branches_in_workspace()?;

        Ok(Self {
            _perm: Some(perm),
            repository,
            new_target,
            old_target,
            virtual_branches_in_workspace,
        })
    }
}

pub fn upstream_integration_statuses(
    context: &UpstreamIntegrationContext,
) -> Result<BranchStatuses> {
    let UpstreamIntegrationContext {
        repository,
        new_target,
        old_target,
        virtual_branches_in_workspace,
        ..
    } = context;
    // look up the target and see if there is a new oid
    let old_target_tree = repository.find_real_tree(old_target, Default::default())?;
    let new_target_tree = repository.find_real_tree(new_target, Default::default())?;

    if new_target.id() == old_target.id() {
        return Ok(BranchStatuses::UpToDate);
    };

    let statuses = virtual_branches_in_workspace
        .iter()
        .map(|virtual_branch| {
            let tree = repository.find_tree(virtual_branch.tree)?;
            let head = repository.find_commit(virtual_branch.head)?;
            let head_tree = repository.find_real_tree(&head, Default::default())?;

            // Try cherry pick the branch's head commit onto the target to
            // see if it conflics. This is equivalent to doing a merge
            // but accounts for the commit being conflicted.

            let has_commits = virtual_branch.head != old_target.id();
            let has_uncommited_changes = head_tree.id() != tree.id();

            // Is the branch completly empty?
            {
                if !has_commits && !has_uncommited_changes {
                    return Ok((virtual_branch.id, BranchStatus::Empty));
                };
            }

            let head_merge_index =
                repository.merge_trees(&old_target_tree, &new_target_tree, &head_tree, None)?;
            let mut tree_merge_index =
                repository.merge_trees(&old_target_tree, &new_target_tree, &tree, None)?;

            // Is the branch conflicted?
            // A branch can't be integrated if its conflicted
            {
                let commits_conflicted = head_merge_index.has_conflicts();

                // See whether uncommited changes are potentially conflicted
                let potentially_conflicted_uncommited_changes = if has_uncommited_changes {
                    // If the commits are conflicted, we can guarentee that the
                    // tree will be conflicted.
                    if commits_conflicted {
                        true
                    } else {
                        tree_merge_index.has_conflicts()
                    }
                } else {
                    // If there are no uncommited changes, then there can't be
                    // any conflicts.
                    false
                };

                if commits_conflicted || potentially_conflicted_uncommited_changes {
                    return Ok((
                        virtual_branch.id,
                        BranchStatus::Conflicted {
                            potentially_conflicted_uncommited_changes,
                        },
                    ));
                }
            }

            // Is the branch fully integrated?
            {
                // We're safe to write the tree as we've ensured it's
                // unconflicted in the previous test.
                let tree_merge_index_tree = tree_merge_index.write_tree_to(repository)?;

                // Identical trees will have the same Oid so we can compare
                // the two
                if tree_merge_index_tree == new_target_tree.id() {
                    return Ok((virtual_branch.id, BranchStatus::FullyIntegrated));
                }
            }

            Ok((virtual_branch.id, BranchStatus::SaflyUpdatable))
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(BranchStatuses::UpdatesRequired(statuses))
}

pub(crate) fn integrate_upstream(
    context: &UpstreamIntegrationContext,
    resolutions: &[Resolution],
) -> Result<()> {
    // Ensure resolutions match current statuses
    {
        let statuses = upstream_integration_statuses(context)?;

        let BranchStatuses::UpdatesRequired(statuses) = statuses else {
            bail!("Branches are all up to date")
        };

        if resolutions.len() != statuses.len() {
            bail!("Chosen resolutions do not match current integration statuses")
        }

        let all_resolutions_match_statuses = resolutions.iter().all(|resolution| {
            let Some(status) = statuses
                .iter()
                .find(|status| status.0 == resolution.branch_id)
            else {
                return false;
            };

            resolution.resolution.resolution_matches_status(&status.1)
        });

        if !all_resolutions_match_statuses {
            bail!("Chosen resolutions do not match current integration statuses")
        }
    }

    Ok(())
}

fn compute_resolutions(
    context: &UpstreamIntegrationContext,
    resolutions: &[Resolution],
) -> Result<Vec<IntegrationResult>> {
    let UpstreamIntegrationContext {
        repository,
        new_target,
        old_target,
        virtual_branches_in_workspace,
        ..
    } = context;

    resolutions
        .iter()
        .map(|resolution| {
            let Some(virtual_branch) = context
                .virtual_branches_in_workspace
                .iter()
                .find(|branch| branch.id == resolution.branch_id)
            else {
                bail!("Failed to find virtual branch");
            };

            match &resolution.resolution {
                StatusResolution::SaflyUpdatable(resolution_approach)
                | StatusResolution::Empty(resolution_approach)
                | StatusResolution::Conflicted {
                    resolution_approach,
                    ..
                } => match resolution_approach {
                    UpdatableResolutionApproach::Unapply => Ok(IntegrationResult::UnapplyBranch),
                    UpdatableResolutionApproach::Merge => {
                        // Make a merge commit on top of the branch commits,
                        // then rebase the tree ontop of that. If the tree ends
                        // up conflicted, commit the tree.
                        todo!();

                        Ok(IntegrationResult::UpdatedObjects {
                            head: todo!(),
                            tree: todo!(),
                        })
                    }
                    UpdatableResolutionApproach::Rebase => {
                        // Rebase the commits, then try rebasing the tree. If
                        // the tree ends up conflicted, commit the tree.

                        // Rebase virtual branches' commits
                        let virtual_branch_commits =
                            repository.l(virtual_branch.head, LogUntil::Commit(new_target.id()))?;

                        let new_head = cherry_rebase_group(
                            repository,
                            new_target.id(),
                            &virtual_branch_commits,
                            true,
                        )?;

                        let head = repository.find_commit(virtual_branch.head)?;
                        let tree = repository.find_tree(virtual_branch.tree)?;

                        // Rebase tree
                        let author_signature = signature(SignaturePurpose::Author)
                            .context("Failed to get gitbutler signature")?;
                        let committer_signature = signature(SignaturePurpose::Committer)
                            .context("Failed to get gitbutler signature")?;
                        let committed_tree = repository.commit(
                            None,
                            &author_signature,
                            &committer_signature,
                            "Uncommited changes",
                            &tree,
                            &[&head],
                        )?;

                        // Rebase commited tree
                        let new_commited_tree =
                            cherry_rebase_group(repository, new_head, &[committed_tree], true)?;
                        let new_commited_tree = repository.find_commit(new_commited_tree)?;

                        if new_commited_tree.is_conflicted() {
                            Ok(IntegrationResult::UpdatedObjects {
                                head: new_commited_tree.id(),
                                tree: repository
                                    .find_real_tree(&new_commited_tree, Default::default())?
                                    .id(),
                            })
                        } else {
                            Ok(IntegrationResult::UpdatedObjects {
                                head: new_head,
                                tree: new_commited_tree.tree_id(),
                            })
                        }
                    }
                },
                StatusResolution::FullyIntegrated => Ok(IntegrationResult::DeleteBranch),
            }
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(vec![])
}

#[cfg(test)]
mod test {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use gitbutler_branch::BranchOwnershipClaims;
    use tempfile::tempdir;
    use uuid::Uuid;

    use super::*;

    fn commit_file<'a>(
        repository: &'a git2::Repository,
        parent: Option<&git2::Commit>,
        files: &[(&str, &str)],
    ) -> git2::Commit<'a> {
        for (file_name, contents) in files {
            fs::write(repository.path().join("..").join(file_name), contents).unwrap();
        }
        let mut index = repository.index().unwrap();
        index
            .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
            .unwrap();

        let signature = git2::Signature::new(
            "Caleb",
            "caleb@gitbutler.com",
            &git2::Time::new(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64,
                0,
            ),
        )
        .unwrap();
        let commit = repository
            .commit(
                None,
                &signature,
                &signature,
                "Committee",
                &repository.find_tree(index.write_tree().unwrap()).unwrap(),
                parent.map(|c| vec![c]).unwrap_or_default().as_slice(),
            )
            .unwrap();

        repository.find_commit(commit).unwrap()
    }

    fn make_branch(head: git2::Oid, tree: git2::Oid) -> Branch {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();

        Branch {
            id: Uuid::new_v4().into(),
            name: "branchy branch".into(),
            notes: "bla bla bla".into(),
            source_refname: None,
            upstream: None,
            upstream_head: None,
            created_timestamp_ms: now,
            updated_timestamp_ms: now,
            tree,
            head,
            ownership: BranchOwnershipClaims::default(),
            order: 0,
            selected_for_changes: None,
            allow_rebasing: true,
            applied: true,
            in_workspace: true,
            not_in_workspace_wip_change_id: None,
            references: vec![],
        }
    }

    #[test]
    fn test_up_to_date_if_head_commits_equivalent() {
        let tempdir = tempdir().unwrap();
        let repository = git2::Repository::init(tempdir.path()).unwrap();
        let initial_commit = commit_file(&repository, None, &[("foo.txt", "bar")]);
        let head_commit = commit_file(&repository, Some(&initial_commit), &[("foo.txt", "baz")]);

        let context = UpstreamIntegrationContext {
            _perm: None,
            old_target: head_commit.clone(),
            new_target: head_commit,
            repository: &repository,
            virtual_branches_in_workspace: vec![],
        };

        assert_eq!(
            upstream_integration_statuses(&context).unwrap(),
            BranchStatuses::UpToDate,
        )
    }

    #[test]
    fn test_updates_required_if_new_head_ahead() {
        let tempdir = tempdir().unwrap();
        let repository = git2::Repository::init(tempdir.path()).unwrap();
        let initial_commit = commit_file(&repository, None, &[("foo.txt", "bar")]);
        let old_target = commit_file(&repository, Some(&initial_commit), &[("foo.txt", "baz")]);
        let new_target = commit_file(&repository, Some(&old_target), &[("foo.txt", "qux")]);

        let context = UpstreamIntegrationContext {
            _perm: None,
            old_target,
            new_target,
            repository: &repository,
            virtual_branches_in_workspace: vec![],
        };

        assert_eq!(
            upstream_integration_statuses(&context).unwrap(),
            BranchStatuses::UpdatesRequired(vec![]),
        )
    }

    #[test]
    fn test_empty_branch() {
        let tempdir = tempdir().unwrap();
        let repository = git2::Repository::init(tempdir.path()).unwrap();
        let initial_commit = commit_file(&repository, None, &[("foo.txt", "bar")]);
        let old_target = commit_file(&repository, Some(&initial_commit), &[("foo.txt", "baz")]);
        let new_target = commit_file(&repository, Some(&old_target), &[("foo.txt", "qux")]);

        let branch = make_branch(old_target.id(), old_target.tree_id());

        let context = UpstreamIntegrationContext {
            _perm: None,
            old_target,
            new_target,
            repository: &repository,
            virtual_branches_in_workspace: vec![branch.clone()],
        };

        assert_eq!(
            upstream_integration_statuses(&context).unwrap(),
            BranchStatuses::UpdatesRequired(vec![(branch.id, BranchStatus::Empty)]),
        )
    }

    #[test]
    fn test_conflicted_head_branch() {
        let tempdir = tempdir().unwrap();
        let repository = git2::Repository::init(tempdir.path()).unwrap();
        let initial_commit = commit_file(&repository, None, &[("foo.txt", "bar")]);
        let old_target = commit_file(&repository, Some(&initial_commit), &[("foo.txt", "baz")]);
        let branch_head = commit_file(&repository, Some(&old_target), &[("foo.txt", "fux")]);
        let new_target = commit_file(&repository, Some(&old_target), &[("foo.txt", "qux")]);

        let branch = make_branch(branch_head.id(), branch_head.tree_id());

        let context = UpstreamIntegrationContext {
            _perm: None,
            old_target,
            new_target,
            repository: &repository,
            virtual_branches_in_workspace: vec![branch.clone()],
        };

        assert_eq!(
            upstream_integration_statuses(&context).unwrap(),
            BranchStatuses::UpdatesRequired(vec![(
                branch.id,
                BranchStatus::Conflicted {
                    potentially_conflicted_uncommited_changes: false
                }
            )]),
        )
    }

    #[test]
    fn test_conflicted_tree_branch() {
        let tempdir = tempdir().unwrap();
        let repository = git2::Repository::init(tempdir.path()).unwrap();
        let initial_commit = commit_file(&repository, None, &[("foo.txt", "bar")]);
        let old_target = commit_file(&repository, Some(&initial_commit), &[("foo.txt", "baz")]);
        let branch_head = commit_file(&repository, Some(&old_target), &[("foo.txt", "fux")]);
        let new_target = commit_file(&repository, Some(&old_target), &[("foo.txt", "qux")]);

        let branch = make_branch(old_target.id(), branch_head.tree_id());

        let context = UpstreamIntegrationContext {
            _perm: None,
            old_target,
            new_target,
            repository: &repository,
            virtual_branches_in_workspace: vec![branch.clone()],
        };

        assert_eq!(
            upstream_integration_statuses(&context).unwrap(),
            BranchStatuses::UpdatesRequired(vec![(
                branch.id,
                BranchStatus::Conflicted {
                    potentially_conflicted_uncommited_changes: true
                }
            )]),
        )
    }

    #[test]
    fn test_conflicted_head_and_tree_branch() {
        let tempdir = tempdir().unwrap();
        let repository = git2::Repository::init(tempdir.path()).unwrap();
        let initial_commit = commit_file(&repository, None, &[("foo.txt", "bar")]);
        let old_target = commit_file(&repository, Some(&initial_commit), &[("foo.txt", "baz")]);
        let branch_head = commit_file(&repository, Some(&old_target), &[("foo.txt", "fux")]);
        let branch_tree = commit_file(&repository, Some(&old_target), &[("foo.txt", "bax")]);
        let new_target = commit_file(&repository, Some(&old_target), &[("foo.txt", "qux")]);

        let branch = make_branch(branch_head.id(), branch_tree.tree_id());

        let context = UpstreamIntegrationContext {
            _perm: None,
            old_target,
            new_target,
            repository: &repository,
            virtual_branches_in_workspace: vec![branch.clone()],
        };

        assert_eq!(
            upstream_integration_statuses(&context).unwrap(),
            BranchStatuses::UpdatesRequired(vec![(
                branch.id,
                BranchStatus::Conflicted {
                    potentially_conflicted_uncommited_changes: true
                }
            )]),
        )
    }

    #[test]
    fn test_integrated() {
        let tempdir = tempdir().unwrap();
        let repository = git2::Repository::init(tempdir.path()).unwrap();
        let initial_commit = commit_file(&repository, None, &[("foo.txt", "bar")]);
        let old_target = commit_file(&repository, Some(&initial_commit), &[("foo.txt", "baz")]);
        let new_target = commit_file(&repository, Some(&old_target), &[("foo.txt", "qux")]);

        let branch = make_branch(new_target.id(), new_target.tree_id());

        let context = UpstreamIntegrationContext {
            _perm: None,
            old_target,
            new_target,
            repository: &repository,
            virtual_branches_in_workspace: vec![branch.clone()],
        };

        assert_eq!(
            upstream_integration_statuses(&context).unwrap(),
            BranchStatuses::UpdatesRequired(vec![(branch.id, BranchStatus::FullyIntegrated)]),
        )
    }

    #[test]
    fn test_integrated_commit_with_uncommited_changes() {
        let tempdir = tempdir().unwrap();
        let repository = git2::Repository::init(tempdir.path()).unwrap();
        let initial_commit =
            commit_file(&repository, None, &[("foo.txt", "bar"), ("bar.txt", "bar")]);
        let old_target = commit_file(
            &repository,
            Some(&initial_commit),
            &[("foo.txt", "baz"), ("bar.txt", "bar")],
        );
        let new_target = commit_file(
            &repository,
            Some(&old_target),
            &[("foo.txt", "qux"), ("bar.txt", "bar")],
        );
        let tree = commit_file(
            &repository,
            Some(&old_target),
            &[("foo.txt", "baz"), ("bar.txt", "qux")],
        );

        let branch = make_branch(new_target.id(), tree.tree_id());

        let context = UpstreamIntegrationContext {
            _perm: None,
            old_target,
            new_target,
            repository: &repository,
            virtual_branches_in_workspace: vec![branch.clone()],
        };

        assert_eq!(
            upstream_integration_statuses(&context).unwrap(),
            BranchStatuses::UpdatesRequired(vec![(branch.id, BranchStatus::SaflyUpdatable)]),
        )
    }

    #[test]
    fn test_safly_updatable() {
        let tempdir = tempdir().unwrap();
        let repository = git2::Repository::init(tempdir.path()).unwrap();
        let initial_commit = commit_file(
            &repository,
            None,
            &[("files-one.txt", "foo"), ("file-two.txt", "foo")],
        );
        let old_target = commit_file(
            &repository,
            Some(&initial_commit),
            &[("file-one.txt", "bar"), ("file-two.txt", "foo")],
        );
        let new_target = commit_file(
            &repository,
            Some(&old_target),
            &[("file-one.txt", "baz"), ("file-two.txt", "foo")],
        );

        let branch_head = commit_file(
            &repository,
            Some(&old_target),
            &[("file-one.txt", "bar"), ("file-two.txt", "bar")],
        );
        let branch_tree = commit_file(
            &repository,
            Some(&branch_head),
            &[("file-one.txt", "bar"), ("file-two.txt", "baz")],
        );

        let branch = make_branch(branch_head.id(), branch_tree.tree_id());

        let context = UpstreamIntegrationContext {
            _perm: None,
            old_target,
            new_target,
            repository: &repository,
            virtual_branches_in_workspace: vec![branch.clone()],
        };

        assert_eq!(
            upstream_integration_statuses(&context).unwrap(),
            BranchStatuses::UpdatesRequired(vec![(branch.id, BranchStatus::SaflyUpdatable)]),
        )
    }
}