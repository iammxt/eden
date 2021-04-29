/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include "eden/fs/store/BackingStore.h"

namespace facebook {
namespace eden {

class Hash;
class LocalStore;

class ReCasBackingStore : public BackingStore {
 public:
  explicit ReCasBackingStore(std::shared_ptr<LocalStore> localStore);
  ~ReCasBackingStore() override;

  folly::SemiFuture<std::unique_ptr<Tree>> getTree(
      const Hash& id,
      ObjectFetchContext& context) override;
  folly::SemiFuture<std::unique_ptr<Blob>> getBlob(
      const Hash& id,
      ObjectFetchContext& context) override;
  folly::SemiFuture<std::unique_ptr<Tree>> getTreeForCommit(
      const Hash& commitID,
      ObjectFetchContext& context) override;
  folly::SemiFuture<std::unique_ptr<Tree>> getTreeForManifest(
      const Hash& commitID,
      const Hash& manifestID,
      ObjectFetchContext& context) override;

 private:
  // Forbidden copy constructor and assignment operator
  ReCasBackingStore(ReCasBackingStore const&) = delete;
  ReCasBackingStore& operator=(ReCasBackingStore const&) = delete;
  std::shared_ptr<LocalStore> localStore_;
};

} // namespace eden
} // namespace facebook