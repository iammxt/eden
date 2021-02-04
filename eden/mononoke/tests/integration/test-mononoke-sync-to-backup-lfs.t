# Copyright (c) Facebook, Inc. and its affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ CACHEDIR=$PWD/cachepath
  $ . "${TEST_FIXTURES}/library.sh"

setup configuration

  $ LFS_THRESHOLD="1000" REPOID=0 REPONAME=orig setup_common_config blob_files
  $ REPOID=1 REPONAME=backup setup_common_config blob_files
  $ cd $TESTTMP

Setup hg repo, create a commit there. No LFS blobs yet.
  $ hginit_treemanifest repo-hg
  $ cd repo-hg
  $ setup_hg_server

  $ echo s > smallfile
  $ hg commit -Aqm "add small file"
  $ cat >> .hg/hgrc <<EOF
  > [extensions]
  > pushrebase =
  > EOF

  $ hg bookmark master_bookmark -r tip
  $ cd ..

Blobimport the hg repo to Mononoke
  $ REPOID=0 blobimport repo-hg/.hg orig
  $ REPONAME=orig
  $ REPOID=1 blobimport repo-hg/.hg backup

Start Mononoke with LFS enabled.
  $ mononoke
  $ wait_for_mononoke

Start Mononoke API server, to serve LFS blobs
  $ lfs_uri="$(lfs_server)/orig"

Create a new client repository. Enable LFS there.
  $ hgclone_treemanifest ssh://user@dummy/repo-hg repo-hg-lfs --noupdate --config extensions.remotenames=
  $ hgclone_treemanifest mononoke://$(mononoke_address)/backup backup --noupdate --config extensions.remotenames=
  $ cd repo-hg-lfs
  $ cat >> .hg/hgrc <<EOF
  > [extensions]
  > pushrebase =
  > remotenames =
  > EOF
  $ setup_hg_lfs "$lfs_uri" 1000B "$TESTTMP/lfs-cache1"


Update in the client repo
  $ hgmn pull -q
  devel-warn: applied empty changegroup at* (glob)
  $ hgmn update -r master_bookmark -q

Perform LFS push
  $ LONG="$(yes A 2>/dev/null | head -c 2000)"
  $ echo "$LONG" > lfs-largefile
  $ hg commit -Aqm "add lfs-large files"
  $ hgmn push -r . --to master_bookmark -v
  pushing rev 99262937f158 to destination ssh://user@dummy/orig bookmark master_bookmark
  searching for changes
  lfs: uploading e2fff2ce58d585b4b0572e0a323f9e7e5f98cc641489e12c03c401d05d0e350d (1.95 KB)
  lfs: processed: e2fff2ce58d585b4b0572e0a323f9e7e5f98cc641489e12c03c401d05d0e350d
  validated revset for rebase
  1 changesets found
  uncompressed size of bundle content:
       206 (changelog)
       282  lfs-largefile
  adding changesets
  adding manifests
  adding file changes
  added 0 changesets with 0 changes to 0 files
  updating bookmark master_bookmark

Check LFS is not in backup
  $ cd "$TESTTMP/backup"
  $ MONONOKE_DIRECT_PEER=True
  $ REPONAME=backup
  $ hgmn pull
  pulling from mononoke://$LOCALIP:*/backup (glob)
  searching for changes
  no changes found
  adding changesets
  * applied empty changegroup * (glob)
  adding manifests
  adding file changes
  added 0 changesets with 0 changes to 0 files
  adding remote bookmark master_bookmark
  $ hg up master_bookmark
  1 files updated, 0 files merged, 0 files removed, 0 files unresolved
  (activating bookmark master_bookmark)
  $ [ -f lfs-largefile ]; echo "$?"
  1

Sync to backup
  $ cd "$TESTTMP"
  $ mononoke_backup_sync backup sync-once 2 --generate-bundles --darkstorm-backup-repo-id 1 2>&1 | grep "successful sync"
  * successful sync of entries [3] (glob)

Check LFS is in backup
  $ cd "$TESTTMP/backup"
  $ hgmn pull
  pulling from mononoke://$LOCALIP:*/backup (glob)
  searching for changes
  adding changesets
  adding manifests
  adding file changes
  added 1 changesets with 0 changes to 0 files
  updating bookmark master_bookmark
  $ hg up master_bookmark
  1 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ [ -f lfs-largefile ]; echo "$?"
  0