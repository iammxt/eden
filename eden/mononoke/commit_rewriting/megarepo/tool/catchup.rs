/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::{anyhow, Error};
use blobrepo::BlobRepo;
use blobrepo_hg::BlobRepoHg;
use blobstore::Loadable;
use bookmarks::BookmarkName;
use context::CoreContext;
use derived_data::BonsaiDerived;
use fsnodes::RootFsnodeId;
use futures::{
    compat::{Future01CompatExt, Stream01CompatExt},
    future::{self, try_join},
    TryStreamExt,
};
use itertools::Itertools;
use manifest::{Diff, ManifestOps};
use maplit::hashset;
use megarepolib::common::{create_and_save_bonsai, ChangesetArgsFactory, StackPosition};
use metaconfig_types::PushrebaseFlags;
use mononoke_types::{ChangesetId, MPath};
use pushrebase::do_pushrebase_bonsai;
use regex::Regex;
use slog::info;

pub async fn create_deletion_head_commits<'a>(
    ctx: &'a CoreContext,
    repo: &'a BlobRepo,
    head_bookmark: BookmarkName,
    commit_to_merge: ChangesetId,
    path_regex: Regex,
    deletion_chunk_size: usize,
    cs_args_factory: Box<dyn ChangesetArgsFactory>,
    pushrebase_flags: &'a PushrebaseFlags,
) -> Result<(), Error> {
    let files =
        find_files_that_need_to_be_deleted(ctx, repo, &head_bookmark, commit_to_merge, path_regex)
            .await?;

    info!(ctx.logger(), "total files to delete is {}", files.len());
    for (num, chunk) in files
        .into_iter()
        .chunks(deletion_chunk_size)
        .into_iter()
        .enumerate()
    {
        let files = chunk.into_iter().map(|path| (path, None)).collect();
        let maybe_head_bookmark_val = repo
            .get_bonsai_bookmark(ctx.clone(), &head_bookmark)
            .compat()
            .await?;
        let head_bookmark_val =
            maybe_head_bookmark_val.ok_or(anyhow!("{} not found", head_bookmark))?;

        let bcs_id = create_and_save_bonsai(
            &ctx,
            &repo,
            vec![head_bookmark_val],
            files,
            cs_args_factory(StackPosition(num)),
        )
        .await?;
        info!(
            ctx.logger(),
            "created bonsai #{}. Deriving hg changeset for it to verify its correctness", num
        );
        let hg_cs_id = repo
            .get_hg_from_bonsai_changeset(ctx.clone(), bcs_id)
            .compat()
            .await?;

        info!(ctx.logger(), "derived {}, pushrebasing...", hg_cs_id);

        let bcs = bcs_id.load(ctx.clone(), repo.blobstore()).await?;
        let pushrebase_res = do_pushrebase_bonsai(
            &ctx,
            &repo,
            pushrebase_flags,
            &head_bookmark,
            &hashset![bcs],
            None,
            &[],
        )
        .await?;
        info!(ctx.logger(), "Pushrebased to {}", pushrebase_res.head);
    }

    Ok(())
}

// Returns paths of the files that:
// 1) Match `path_regex`
// 2) Either do not exist in `commit_to_merge` or have different content/filetype.
async fn find_files_that_need_to_be_deleted(
    ctx: &CoreContext,
    repo: &BlobRepo,
    head_bookmark: &BookmarkName,
    commit_to_merge: ChangesetId,
    path_regex: Regex,
) -> Result<Vec<MPath>, Error> {
    let maybe_head_bookmark_val = repo
        .get_bonsai_bookmark(ctx.clone(), head_bookmark)
        .compat()
        .await?;

    let head_bookmark_val =
        maybe_head_bookmark_val.ok_or(anyhow!("{} not found", head_bookmark))?;

    let head_root_fsnode = RootFsnodeId::derive(ctx.clone(), repo.clone(), head_bookmark_val);
    let commit_to_merge_root_fsnode =
        RootFsnodeId::derive(ctx.clone(), repo.clone(), commit_to_merge);

    let (head_root_fsnode, commit_to_merge_root_fsnode) = try_join(
        head_root_fsnode.compat(),
        commit_to_merge_root_fsnode.compat(),
    )
    .await?;

    let paths = head_root_fsnode
        .fsnode_id()
        .diff(
            ctx.clone(),
            repo.get_blobstore(),
            *commit_to_merge_root_fsnode.fsnode_id(),
        )
        .compat()
        .try_filter_map(|diff| async move {
            use Diff::*;
            let maybe_path = match diff {
                Added(_maybe_path, _entry) => None,
                Removed(maybe_path, entry) => entry.into_leaf().and_then(|_| maybe_path),
                Changed(maybe_path, _old_entry, new_entry) => {
                    new_entry.into_leaf().and_then(|_| maybe_path)
                }
            };

            Ok(maybe_path)
        })
        .try_filter(|path| future::ready(path.matches_regex(&path_regex)))
        .try_collect::<Vec<_>>()
        .await?;

    Ok(paths)
}

#[cfg(test)]
mod test {
    use super::*;
    use fbinit::FacebookInit;
    use megarepolib::common::ChangesetArgs;
    use mononoke_types::DateTime;
    use revset::RangeNodeStream;
    use tests_utils::{bookmark, resolve_cs_id, CreateCommitContext};

    const PATH_REGEX: &'static str = "^(unchanged/.*|changed/.*|toremove/.*)";

    #[fbinit::compat_test]
    async fn test_find_files_that_needs_to_be_deleted(fb: FacebookInit) -> Result<(), Error> {
        let ctx = CoreContext::test_mock(fb);
        let repo = prepare_repo(&ctx).await?;

        let commit_to_merge = resolve_cs_id(&ctx, &repo, "commit_to_merge").await?;
        let book = BookmarkName::new("book")?;
        let mut paths = find_files_that_need_to_be_deleted(
            &ctx,
            &repo,
            &book,
            commit_to_merge,
            Regex::new(PATH_REGEX)?,
        )
        .await?;

        paths.sort();
        assert_eq!(
            paths,
            vec![
                MPath::new("changed/a")?,
                MPath::new("changed/b")?,
                MPath::new("toremove/file1")?,
                MPath::new("toremove/file2")?,
            ]
        );

        Ok(())
    }

    #[fbinit::compat_test]
    async fn test_create_deletion_head_commits(fb: FacebookInit) -> Result<(), Error> {
        let ctx = CoreContext::test_mock(fb);
        let repo = prepare_repo(&ctx).await?;
        let book = BookmarkName::new("book")?;

        let commit_to_merge = resolve_cs_id(&ctx, &repo, "commit_to_merge").await?;
        let args_factory = Box::new(|stack_pos: StackPosition| ChangesetArgs {
            author: "author".to_string(),
            message: format!("{}", stack_pos.0),
            datetime: DateTime::now(),
            bookmark: None,
            mark_public: false,
        });

        let pushrebase_flags = {
            let mut flags = PushrebaseFlags::default();
            flags.rewritedates = true;
            flags.forbid_p2_root_rebases = true;
            flags.casefolding_check = true;
            flags.recursion_limit = None;
            flags
        };

        let commit_before_push = resolve_cs_id(&ctx, &repo, book.clone()).await?;
        create_deletion_head_commits(
            &ctx,
            &repo,
            book.clone(),
            commit_to_merge,
            Regex::new(PATH_REGEX)?,
            1,
            args_factory,
            &pushrebase_flags,
        )
        .await?;
        let commit_after_push = resolve_cs_id(&ctx, &repo, book.clone()).await?;

        let range: Vec<_> = RangeNodeStream::new(
            ctx.clone(),
            repo.get_changeset_fetcher(),
            commit_before_push,
            commit_after_push,
        )
        .compat()
        .try_collect()
        .await?;
        // 4 new commits + commit_before_push
        assert_eq!(range.len(), 4 + 1);

        let paths = find_files_that_need_to_be_deleted(
            &ctx,
            &repo,
            &book,
            commit_to_merge,
            Regex::new(PATH_REGEX)?,
        )
        .await?;

        assert!(paths.is_empty());
        Ok(())
    }

    async fn prepare_repo(ctx: &CoreContext) -> Result<BlobRepo, Error> {
        let repo = blobrepo_factory::new_memblob_empty(None)?;

        let head_commit = CreateCommitContext::new_root(ctx, &repo)
            .add_file("unrelated_file", "a")
            .add_file("unchanged/a", "a")
            .add_file("changed/a", "oldcontent")
            .add_file("changed/b", "oldcontent")
            .add_file("toremove/file1", "content")
            .add_file("toremove/file2", "content")
            .commit()
            .await?;

        let commit_to_merge = CreateCommitContext::new_root(ctx, &repo)
            .add_file("unchanged/a", "a")
            .add_file("changed/a", "newcontent")
            .add_file("changed/b", "newcontent")
            .commit()
            .await?;

        bookmark(&ctx, &repo, "book").set_to(head_commit).await?;
        bookmark(&ctx, &repo, "commit_to_merge")
            .set_to(commit_to_merge)
            .await?;

        Ok(repo)
    }
}