use crate::{
    worktree_settings::WorktreeSettings, Entry, EntryKind, Event, PathChange, StatusEntry,
    WorkDirectory, Worktree, WorktreeModelHandle,
};
use anyhow::Result;
use fs::{FakeFs, Fs, RealFs, RemoveOptions};
use git::{
    repository::RepoPath,
    status::{FileStatus, StatusCode, TrackedStatus},
    GITIGNORE,
};
use git2::RepositoryInitOptions;
use gpui::{AppContext as _, BorrowAppContext, Context, Task, TestAppContext};
use parking_lot::Mutex;
use postage::stream::Stream;
use pretty_assertions::assert_eq;
use rand::prelude::*;

use serde_json::json;
use settings::{Settings, SettingsStore};
use std::{
    env,
    fmt::Write,
    mem,
    path::{Path, PathBuf},
    sync::Arc,
};
use util::{path, test::TempTree, ResultExt};

#[gpui::test]
async fn test_traversal(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.background_executor.clone());
    fs.insert_tree(
        "/root",
        json!({
           ".gitignore": "a/b\n",
           "a": {
               "b": "",
               "c": "",
           }
        }),
    )
    .await;

    let tree = Worktree::local(
        Path::new("/root"),
        true,
        fs,
        Default::default(),
        &mut cx.to_async(),
    )
    .await
    .unwrap();
    cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
        .await;

    tree.read_with(cx, |tree, _| {
        assert_eq!(
            tree.entries(false, 0)
                .map(|entry| entry.path.as_ref())
                .collect::<Vec<_>>(),
            vec![
                Path::new(""),
                Path::new(".gitignore"),
                Path::new("a"),
                Path::new("a/c"),
            ]
        );
        assert_eq!(
            tree.entries(true, 0)
                .map(|entry| entry.path.as_ref())
                .collect::<Vec<_>>(),
            vec![
                Path::new(""),
                Path::new(".gitignore"),
                Path::new("a"),
                Path::new("a/b"),
                Path::new("a/c"),
            ]
        );
    })
}

#[gpui::test(iterations = 10)]
async fn test_circular_symlinks(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.background_executor.clone());
    fs.insert_tree(
        "/root",
        json!({
            "lib": {
                "a": {
                    "a.txt": ""
                },
                "b": {
                    "b.txt": ""
                }
            }
        }),
    )
    .await;
    fs.create_symlink("/root/lib/a/lib".as_ref(), "..".into())
        .await
        .unwrap();
    fs.create_symlink("/root/lib/b/lib".as_ref(), "..".into())
        .await
        .unwrap();

    let tree = Worktree::local(
        Path::new("/root"),
        true,
        fs.clone(),
        Default::default(),
        &mut cx.to_async(),
    )
    .await
    .unwrap();

    cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
        .await;

    tree.read_with(cx, |tree, _| {
        assert_eq!(
            tree.entries(false, 0)
                .map(|entry| entry.path.as_ref())
                .collect::<Vec<_>>(),
            vec![
                Path::new(""),
                Path::new("lib"),
                Path::new("lib/a"),
                Path::new("lib/a/a.txt"),
                Path::new("lib/a/lib"),
                Path::new("lib/b"),
                Path::new("lib/b/b.txt"),
                Path::new("lib/b/lib"),
            ]
        );
    });

    fs.rename(
        Path::new("/root/lib/a/lib"),
        Path::new("/root/lib/a/lib-2"),
        Default::default(),
    )
    .await
    .unwrap();
    cx.executor().run_until_parked();
    tree.read_with(cx, |tree, _| {
        assert_eq!(
            tree.entries(false, 0)
                .map(|entry| entry.path.as_ref())
                .collect::<Vec<_>>(),
            vec![
                Path::new(""),
                Path::new("lib"),
                Path::new("lib/a"),
                Path::new("lib/a/a.txt"),
                Path::new("lib/a/lib-2"),
                Path::new("lib/b"),
                Path::new("lib/b/b.txt"),
                Path::new("lib/b/lib"),
            ]
        );
    });
}

#[gpui::test]
async fn test_symlinks_pointing_outside(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.background_executor.clone());
    fs.insert_tree(
        "/root",
        json!({
            "dir1": {
                "deps": {
                    // symlinks here
                },
                "src": {
                    "a.rs": "",
                    "b.rs": "",
                },
            },
            "dir2": {
                "src": {
                    "c.rs": "",
                    "d.rs": "",
                }
            },
            "dir3": {
                "deps": {},
                "src": {
                    "e.rs": "",
                    "f.rs": "",
                },
            }
        }),
    )
    .await;

    // These symlinks point to directories outside of the worktree's root, dir1.
    fs.create_symlink("/root/dir1/deps/dep-dir2".as_ref(), "../../dir2".into())
        .await
        .unwrap();
    fs.create_symlink("/root/dir1/deps/dep-dir3".as_ref(), "../../dir3".into())
        .await
        .unwrap();

    let tree = Worktree::local(
        Path::new("/root/dir1"),
        true,
        fs.clone(),
        Default::default(),
        &mut cx.to_async(),
    )
    .await
    .unwrap();

    cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
        .await;

    let tree_updates = Arc::new(Mutex::new(Vec::new()));
    tree.update(cx, |_, cx| {
        let tree_updates = tree_updates.clone();
        cx.subscribe(&tree, move |_, _, event, _| {
            if let Event::UpdatedEntries(update) = event {
                tree_updates.lock().extend(
                    update
                        .iter()
                        .map(|(path, _, change)| (path.clone(), *change)),
                );
            }
        })
        .detach();
    });

    // The symlinked directories are not scanned by default.
    tree.read_with(cx, |tree, _| {
        assert_eq!(
            tree.entries(true, 0)
                .map(|entry| (entry.path.as_ref(), entry.is_external))
                .collect::<Vec<_>>(),
            vec![
                (Path::new(""), false),
                (Path::new("deps"), false),
                (Path::new("deps/dep-dir2"), true),
                (Path::new("deps/dep-dir3"), true),
                (Path::new("src"), false),
                (Path::new("src/a.rs"), false),
                (Path::new("src/b.rs"), false),
            ]
        );

        assert_eq!(
            tree.entry_for_path("deps/dep-dir2").unwrap().kind,
            EntryKind::UnloadedDir
        );
    });

    // Expand one of the symlinked directories.
    tree.read_with(cx, |tree, _| {
        tree.as_local()
            .unwrap()
            .refresh_entries_for_paths(vec![Path::new("deps/dep-dir3").into()])
    })
    .recv()
    .await;

    // The expanded directory's contents are loaded. Subdirectories are
    // not scanned yet.
    tree.read_with(cx, |tree, _| {
        assert_eq!(
            tree.entries(true, 0)
                .map(|entry| (entry.path.as_ref(), entry.is_external))
                .collect::<Vec<_>>(),
            vec![
                (Path::new(""), false),
                (Path::new("deps"), false),
                (Path::new("deps/dep-dir2"), true),
                (Path::new("deps/dep-dir3"), true),
                (Path::new("deps/dep-dir3/deps"), true),
                (Path::new("deps/dep-dir3/src"), true),
                (Path::new("src"), false),
                (Path::new("src/a.rs"), false),
                (Path::new("src/b.rs"), false),
            ]
        );
    });
    assert_eq!(
        mem::take(&mut *tree_updates.lock()),
        &[
            (Path::new("deps/dep-dir3").into(), PathChange::Loaded),
            (Path::new("deps/dep-dir3/deps").into(), PathChange::Loaded),
            (Path::new("deps/dep-dir3/src").into(), PathChange::Loaded)
        ]
    );

    // Expand a subdirectory of one of the symlinked directories.
    tree.read_with(cx, |tree, _| {
        tree.as_local()
            .unwrap()
            .refresh_entries_for_paths(vec![Path::new("deps/dep-dir3/src").into()])
    })
    .recv()
    .await;

    // The expanded subdirectory's contents are loaded.
    tree.read_with(cx, |tree, _| {
        assert_eq!(
            tree.entries(true, 0)
                .map(|entry| (entry.path.as_ref(), entry.is_external))
                .collect::<Vec<_>>(),
            vec![
                (Path::new(""), false),
                (Path::new("deps"), false),
                (Path::new("deps/dep-dir2"), true),
                (Path::new("deps/dep-dir3"), true),
                (Path::new("deps/dep-dir3/deps"), true),
                (Path::new("deps/dep-dir3/src"), true),
                (Path::new("deps/dep-dir3/src/e.rs"), true),
                (Path::new("deps/dep-dir3/src/f.rs"), true),
                (Path::new("src"), false),
                (Path::new("src/a.rs"), false),
                (Path::new("src/b.rs"), false),
            ]
        );
    });

    assert_eq!(
        mem::take(&mut *tree_updates.lock()),
        &[
            (Path::new("deps/dep-dir3/src").into(), PathChange::Loaded),
            (
                Path::new("deps/dep-dir3/src/e.rs").into(),
                PathChange::Loaded
            ),
            (
                Path::new("deps/dep-dir3/src/f.rs").into(),
                PathChange::Loaded
            )
        ]
    );
}

#[cfg(target_os = "macos")]
#[gpui::test]
async fn test_renaming_case_only(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    init_test(cx);

    const OLD_NAME: &str = "aaa.rs";
    const NEW_NAME: &str = "AAA.rs";

    let fs = Arc::new(RealFs::new(None, cx.executor()));
    let temp_root = TempTree::new(json!({
        OLD_NAME: "",
    }));

    let tree = Worktree::local(
        temp_root.path(),
        true,
        fs.clone(),
        Default::default(),
        &mut cx.to_async(),
    )
    .await
    .unwrap();

    cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
        .await;
    tree.read_with(cx, |tree, _| {
        assert_eq!(
            tree.entries(true, 0)
                .map(|entry| entry.path.as_ref())
                .collect::<Vec<_>>(),
            vec![Path::new(""), Path::new(OLD_NAME)]
        );
    });

    fs.rename(
        &temp_root.path().join(OLD_NAME),
        &temp_root.path().join(NEW_NAME),
        fs::RenameOptions {
            overwrite: true,
            ignore_if_exists: true,
        },
    )
    .await
    .unwrap();

    tree.flush_fs_events(cx).await;

    tree.read_with(cx, |tree, _| {
        assert_eq!(
            tree.entries(true, 0)
                .map(|entry| entry.path.as_ref())
                .collect::<Vec<_>>(),
            vec![Path::new(""), Path::new(NEW_NAME)]
        );
    });
}

#[gpui::test]
async fn test_open_gitignored_files(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.background_executor.clone());
    fs.insert_tree(
        "/root",
        json!({
            ".gitignore": "node_modules\n",
            "one": {
                "node_modules": {
                    "a": {
                        "a1.js": "a1",
                        "a2.js": "a2",
                    },
                    "b": {
                        "b1.js": "b1",
                        "b2.js": "b2",
                    },
                    "c": {
                        "c1.js": "c1",
                        "c2.js": "c2",
                    }
                },
            },
            "two": {
                "x.js": "",
                "y.js": "",
            },
        }),
    )
    .await;

    let tree = Worktree::local(
        Path::new("/root"),
        true,
        fs.clone(),
        Default::default(),
        &mut cx.to_async(),
    )
    .await
    .unwrap();

    cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
        .await;

    tree.read_with(cx, |tree, _| {
        assert_eq!(
            tree.entries(true, 0)
                .map(|entry| (entry.path.as_ref(), entry.is_ignored))
                .collect::<Vec<_>>(),
            vec![
                (Path::new(""), false),
                (Path::new(".gitignore"), false),
                (Path::new("one"), false),
                (Path::new("one/node_modules"), true),
                (Path::new("two"), false),
                (Path::new("two/x.js"), false),
                (Path::new("two/y.js"), false),
            ]
        );
    });

    // Open a file that is nested inside of a gitignored directory that
    // has not yet been expanded.
    let prev_read_dir_count = fs.read_dir_call_count();
    let loaded = tree
        .update(cx, |tree, cx| {
            tree.load_file("one/node_modules/b/b1.js".as_ref(), cx)
        })
        .await
        .unwrap();

    tree.read_with(cx, |tree, _| {
        assert_eq!(
            tree.entries(true, 0)
                .map(|entry| (entry.path.as_ref(), entry.is_ignored))
                .collect::<Vec<_>>(),
            vec![
                (Path::new(""), false),
                (Path::new(".gitignore"), false),
                (Path::new("one"), false),
                (Path::new("one/node_modules"), true),
                (Path::new("one/node_modules/a"), true),
                (Path::new("one/node_modules/b"), true),
                (Path::new("one/node_modules/b/b1.js"), true),
                (Path::new("one/node_modules/b/b2.js"), true),
                (Path::new("one/node_modules/c"), true),
                (Path::new("two"), false),
                (Path::new("two/x.js"), false),
                (Path::new("two/y.js"), false),
            ]
        );

        assert_eq!(
            loaded.file.path.as_ref(),
            Path::new("one/node_modules/b/b1.js")
        );

        // Only the newly-expanded directories are scanned.
        assert_eq!(fs.read_dir_call_count() - prev_read_dir_count, 2);
    });

    // Open another file in a different subdirectory of the same
    // gitignored directory.
    let prev_read_dir_count = fs.read_dir_call_count();
    let loaded = tree
        .update(cx, |tree, cx| {
            tree.load_file("one/node_modules/a/a2.js".as_ref(), cx)
        })
        .await
        .unwrap();

    tree.read_with(cx, |tree, _| {
        assert_eq!(
            tree.entries(true, 0)
                .map(|entry| (entry.path.as_ref(), entry.is_ignored))
                .collect::<Vec<_>>(),
            vec![
                (Path::new(""), false),
                (Path::new(".gitignore"), false),
                (Path::new("one"), false),
                (Path::new("one/node_modules"), true),
                (Path::new("one/node_modules/a"), true),
                (Path::new("one/node_modules/a/a1.js"), true),
                (Path::new("one/node_modules/a/a2.js"), true),
                (Path::new("one/node_modules/b"), true),
                (Path::new("one/node_modules/b/b1.js"), true),
                (Path::new("one/node_modules/b/b2.js"), true),
                (Path::new("one/node_modules/c"), true),
                (Path::new("two"), false),
                (Path::new("two/x.js"), false),
                (Path::new("two/y.js"), false),
            ]
        );

        assert_eq!(
            loaded.file.path.as_ref(),
            Path::new("one/node_modules/a/a2.js")
        );

        // Only the newly-expanded directory is scanned.
        assert_eq!(fs.read_dir_call_count() - prev_read_dir_count, 1);
    });

    let path = PathBuf::from("/root/one/node_modules/c/lib");

    // No work happens when files and directories change within an unloaded directory.
    let prev_fs_call_count = fs.read_dir_call_count() + fs.metadata_call_count();
    // When we open a directory, we check each ancestor whether it's a git
    // repository. That means we have an fs.metadata call per ancestor that we
    // need to subtract here.
    let ancestors = path.ancestors().count();

    fs.create_dir(path.as_ref()).await.unwrap();
    cx.executor().run_until_parked();

    assert_eq!(
        fs.read_dir_call_count() + fs.metadata_call_count() - prev_fs_call_count - ancestors,
        0
    );
}

#[gpui::test]
async fn test_dirs_no_longer_ignored(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.background_executor.clone());
    fs.insert_tree(
        "/root",
        json!({
            ".gitignore": "node_modules\n",
            "a": {
                "a.js": "",
            },
            "b": {
                "b.js": "",
            },
            "node_modules": {
                "c": {
                    "c.js": "",
                },
                "d": {
                    "d.js": "",
                    "e": {
                        "e1.js": "",
                        "e2.js": "",
                    },
                    "f": {
                        "f1.js": "",
                        "f2.js": "",
                    }
                },
            },
        }),
    )
    .await;

    let tree = Worktree::local(
        Path::new("/root"),
        true,
        fs.clone(),
        Default::default(),
        &mut cx.to_async(),
    )
    .await
    .unwrap();

    cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
        .await;

    // Open a file within the gitignored directory, forcing some of its
    // subdirectories to be read, but not all.
    let read_dir_count_1 = fs.read_dir_call_count();
    tree.read_with(cx, |tree, _| {
        tree.as_local()
            .unwrap()
            .refresh_entries_for_paths(vec![Path::new("node_modules/d/d.js").into()])
    })
    .recv()
    .await;

    // Those subdirectories are now loaded.
    tree.read_with(cx, |tree, _| {
        assert_eq!(
            tree.entries(true, 0)
                .map(|e| (e.path.as_ref(), e.is_ignored))
                .collect::<Vec<_>>(),
            &[
                (Path::new(""), false),
                (Path::new(".gitignore"), false),
                (Path::new("a"), false),
                (Path::new("a/a.js"), false),
                (Path::new("b"), false),
                (Path::new("b/b.js"), false),
                (Path::new("node_modules"), true),
                (Path::new("node_modules/c"), true),
                (Path::new("node_modules/d"), true),
                (Path::new("node_modules/d/d.js"), true),
                (Path::new("node_modules/d/e"), true),
                (Path::new("node_modules/d/f"), true),
            ]
        );
    });
    let read_dir_count_2 = fs.read_dir_call_count();
    assert_eq!(read_dir_count_2 - read_dir_count_1, 2);

    // Update the gitignore so that node_modules is no longer ignored,
    // but a subdirectory is ignored
    fs.save("/root/.gitignore".as_ref(), &"e".into(), Default::default())
        .await
        .unwrap();
    cx.executor().run_until_parked();

    // All of the directories that are no longer ignored are now loaded.
    tree.read_with(cx, |tree, _| {
        assert_eq!(
            tree.entries(true, 0)
                .map(|e| (e.path.as_ref(), e.is_ignored))
                .collect::<Vec<_>>(),
            &[
                (Path::new(""), false),
                (Path::new(".gitignore"), false),
                (Path::new("a"), false),
                (Path::new("a/a.js"), false),
                (Path::new("b"), false),
                (Path::new("b/b.js"), false),
                // This directory is no longer ignored
                (Path::new("node_modules"), false),
                (Path::new("node_modules/c"), false),
                (Path::new("node_modules/c/c.js"), false),
                (Path::new("node_modules/d"), false),
                (Path::new("node_modules/d/d.js"), false),
                // This subdirectory is now ignored
                (Path::new("node_modules/d/e"), true),
                (Path::new("node_modules/d/f"), false),
                (Path::new("node_modules/d/f/f1.js"), false),
                (Path::new("node_modules/d/f/f2.js"), false),
            ]
        );
    });

    // Each of the newly-loaded directories is scanned only once.
    let read_dir_count_3 = fs.read_dir_call_count();
    assert_eq!(read_dir_count_3 - read_dir_count_2, 2);
}

#[gpui::test(iterations = 10)]
async fn test_rescan_with_gitignore(cx: &mut TestAppContext) {
    init_test(cx);
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings::<WorktreeSettings>(cx, |project_settings| {
                project_settings.file_scan_exclusions = Some(Vec::new());
            });
        });
    });
    let fs = FakeFs::new(cx.background_executor.clone());
    fs.insert_tree(
        path!("/root"),
        json!({
            ".gitignore": "ancestor-ignored-file1\nancestor-ignored-file2\n",
            "tree": {
                ".git": {},
                ".gitignore": "ignored-dir\n",
                "tracked-dir": {
                    "tracked-file1": "",
                    "ancestor-ignored-file1": "",
                },
                "ignored-dir": {
                    "ignored-file1": ""
                }
            }
        }),
    )
    .await;
    fs.set_head_and_index_for_repo(
        path!("/root/tree/.git").as_ref(),
        &[
            (".gitignore".into(), "ignored-dir\n".into()),
            ("tracked-dir/tracked-file1".into(), "".into()),
        ],
    );

    let tree = Worktree::local(
        path!("/root/tree").as_ref(),
        true,
        fs.clone(),
        Default::default(),
        &mut cx.to_async(),
    )
    .await
    .unwrap();
    cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
        .await;

    tree.read_with(cx, |tree, _| {
        tree.as_local()
            .unwrap()
            .refresh_entries_for_paths(vec![Path::new("ignored-dir").into()])
    })
    .recv()
    .await;

    cx.read(|cx| {
        let tree = tree.read(cx);
        assert_entry_git_state(tree, "tracked-dir/tracked-file1", None, false);
        assert_entry_git_state(tree, "tracked-dir/ancestor-ignored-file1", None, false);
        assert_entry_git_state(tree, "ignored-dir/ignored-file1", None, true);
    });

    fs.create_file(
        path!("/root/tree/tracked-dir/tracked-file2").as_ref(),
        Default::default(),
    )
    .await
    .unwrap();
    fs.set_index_for_repo(
        path!("/root/tree/.git").as_ref(),
        &[
            (".gitignore".into(), "ignored-dir\n".into()),
            ("tracked-dir/tracked-file1".into(), "".into()),
            ("tracked-dir/tracked-file2".into(), "".into()),
        ],
    );
    fs.create_file(
        path!("/root/tree/tracked-dir/ancestor-ignored-file2").as_ref(),
        Default::default(),
    )
    .await
    .unwrap();
    fs.create_file(
        path!("/root/tree/ignored-dir/ignored-file2").as_ref(),
        Default::default(),
    )
    .await
    .unwrap();

    cx.executor().run_until_parked();
    cx.read(|cx| {
        let tree = tree.read(cx);
        assert_entry_git_state(
            tree,
            "tracked-dir/tracked-file2",
            Some(StatusCode::Added),
            false,
        );
        assert_entry_git_state(tree, "tracked-dir/ancestor-ignored-file2", None, false);
        assert_entry_git_state(tree, "ignored-dir/ignored-file2", None, true);
        assert!(tree.entry_for_path(".git").unwrap().is_ignored);
    });
}

#[gpui::test]
async fn test_update_gitignore(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.background_executor.clone());
    fs.insert_tree(
        path!("/root"),
        json!({
            ".git": {},
            ".gitignore": "*.txt\n",
            "a.xml": "<a></a>",
            "b.txt": "Some text"
        }),
    )
    .await;

    fs.set_head_and_index_for_repo(
        path!("/root/.git").as_ref(),
        &[
            (".gitignore".into(), "*.txt\n".into()),
            ("a.xml".into(), "<a></a>".into()),
        ],
    );

    let tree = Worktree::local(
        path!("/root").as_ref(),
        true,
        fs.clone(),
        Default::default(),
        &mut cx.to_async(),
    )
    .await
    .unwrap();
    cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
        .await;

    tree.read_with(cx, |tree, _| {
        tree.as_local()
            .unwrap()
            .refresh_entries_for_paths(vec![Path::new("").into()])
    })
    .recv()
    .await;

    // One file is unmodified, the other is ignored.
    cx.read(|cx| {
        let tree = tree.read(cx);
        assert_entry_git_state(tree, "a.xml", None, false);
        assert_entry_git_state(tree, "b.txt", None, true);
    });

    // Change the gitignore, and stage the newly non-ignored file.
    fs.atomic_write(path!("/root/.gitignore").into(), "*.xml\n".into())
        .await
        .unwrap();
    fs.set_index_for_repo(
        Path::new(path!("/root/.git")),
        &[
            (".gitignore".into(), "*.txt\n".into()),
            ("a.xml".into(), "<a></a>".into()),
            ("b.txt".into(), "Some text".into()),
        ],
    );

    cx.executor().run_until_parked();
    cx.read(|cx| {
        let tree = tree.read(cx);
        assert_entry_git_state(tree, "a.xml", None, true);
        assert_entry_git_state(tree, "b.txt", Some(StatusCode::Added), false);
    });
}

#[gpui::test]
async fn test_write_file(cx: &mut TestAppContext) {
    init_test(cx);
    cx.executor().allow_parking();
    let dir = TempTree::new(json!({
        ".git": {},
        ".gitignore": "ignored-dir\n",
        "tracked-dir": {},
        "ignored-dir": {}
    }));

    let worktree = Worktree::local(
        dir.path(),
        true,
        Arc::new(RealFs::new(None, cx.executor())),
        Default::default(),
        &mut cx.to_async(),
    )
    .await
    .unwrap();

    #[cfg(not(target_os = "macos"))]
    fs::fs_watcher::global(|_| {}).unwrap();

    cx.read(|cx| worktree.read(cx).as_local().unwrap().scan_complete())
        .await;
    worktree.flush_fs_events(cx).await;

    worktree
        .update(cx, |tree, cx| {
            tree.write_file(
                Path::new("tracked-dir/file.txt"),
                "hello".into(),
                Default::default(),
                cx,
            )
        })
        .await
        .unwrap();
    worktree
        .update(cx, |tree, cx| {
            tree.write_file(
                Path::new("ignored-dir/file.txt"),
                "world".into(),
                Default::default(),
                cx,
            )
        })
        .await
        .unwrap();

    worktree.read_with(cx, |tree, _| {
        let tracked = tree.entry_for_path("tracked-dir/file.txt").unwrap();
        let ignored = tree.entry_for_path("ignored-dir/file.txt").unwrap();
        assert!(!tracked.is_ignored);
        assert!(ignored.is_ignored);
    });
}

#[gpui::test]
async fn test_file_scan_inclusions(cx: &mut TestAppContext) {
    init_test(cx);
    cx.executor().allow_parking();
    let dir = TempTree::new(json!({
        ".gitignore": "**/target\n/node_modules\ntop_level.txt\n",
        "target": {
            "index": "blah2"
        },
        "node_modules": {
            ".DS_Store": "",
            "prettier": {
                "package.json": "{}",
            },
        },
        "src": {
            ".DS_Store": "",
            "foo": {
                "foo.rs": "mod another;\n",
                "another.rs": "// another",
            },
            "bar": {
                "bar.rs": "// bar",
            },
            "lib.rs": "mod foo;\nmod bar;\n",
        },
        "top_level.txt": "top level file",
        ".DS_Store": "",
    }));
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings::<WorktreeSettings>(cx, |project_settings| {
                project_settings.file_scan_exclusions = Some(vec![]);
                project_settings.file_scan_inclusions = Some(vec![
                    "node_modules/**/package.json".to_string(),
                    "**/.DS_Store".to_string(),
                ]);
            });
        });
    });

    let tree = Worktree::local(
        dir.path(),
        true,
        Arc::new(RealFs::new(None, cx.executor())),
        Default::default(),
        &mut cx.to_async(),
    )
    .await
    .unwrap();
    cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
        .await;
    tree.flush_fs_events(cx).await;
    tree.read_with(cx, |tree, _| {
        // Assert that file_scan_inclusions overrides  file_scan_exclusions.
        check_worktree_entries(
            tree,
            &[],
            &["target", "node_modules"],
            &["src/lib.rs", "src/bar/bar.rs", ".gitignore"],
            &[
                "node_modules/prettier/package.json",
                ".DS_Store",
                "node_modules/.DS_Store",
                "src/.DS_Store",
            ],
        )
    });
}

#[gpui::test]
async fn test_file_scan_exclusions_overrules_inclusions(cx: &mut TestAppContext) {
    init_test(cx);
    cx.executor().allow_parking();
    let dir = TempTree::new(json!({
        ".gitignore": "**/target\n/node_modules\n",
        "target": {
            "index": "blah2"
        },
        "node_modules": {
            ".DS_Store": "",
            "prettier": {
                "package.json": "{}",
            },
        },
        "src": {
            ".DS_Store": "",
            "foo": {
                "foo.rs": "mod another;\n",
                "another.rs": "// another",
            },
        },
        ".DS_Store": "",
    }));

    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings::<WorktreeSettings>(cx, |project_settings| {
                project_settings.file_scan_exclusions = Some(vec!["**/.DS_Store".to_string()]);
                project_settings.file_scan_inclusions = Some(vec!["**/.DS_Store".to_string()]);
            });
        });
    });

    let tree = Worktree::local(
        dir.path(),
        true,
        Arc::new(RealFs::new(None, cx.executor())),
        Default::default(),
        &mut cx.to_async(),
    )
    .await
    .unwrap();
    cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
        .await;
    tree.flush_fs_events(cx).await;
    tree.read_with(cx, |tree, _| {
        // Assert that file_scan_inclusions overrides  file_scan_exclusions.
        check_worktree_entries(
            tree,
            &[".DS_Store, src/.DS_Store"],
            &["target", "node_modules"],
            &["src/foo/another.rs", "src/foo/foo.rs", ".gitignore"],
            &[],
        )
    });
}

#[gpui::test]
async fn test_file_scan_inclusions_reindexes_on_setting_change(cx: &mut TestAppContext) {
    init_test(cx);
    cx.executor().allow_parking();
    let dir = TempTree::new(json!({
        ".gitignore": "**/target\n/node_modules/\n",
        "target": {
            "index": "blah2"
        },
        "node_modules": {
            ".DS_Store": "",
            "prettier": {
                "package.json": "{}",
            },
        },
        "src": {
            ".DS_Store": "",
            "foo": {
                "foo.rs": "mod another;\n",
                "another.rs": "// another",
            },
        },
        ".DS_Store": "",
    }));

    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings::<WorktreeSettings>(cx, |project_settings| {
                project_settings.file_scan_exclusions = Some(vec![]);
                project_settings.file_scan_inclusions = Some(vec!["node_modules/**".to_string()]);
            });
        });
    });
    let tree = Worktree::local(
        dir.path(),
        true,
        Arc::new(RealFs::new(None, cx.executor())),
        Default::default(),
        &mut cx.to_async(),
    )
    .await
    .unwrap();
    cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
        .await;
    tree.flush_fs_events(cx).await;

    tree.read_with(cx, |tree, _| {
        assert!(tree
            .entry_for_path("node_modules")
            .is_some_and(|f| f.is_always_included));
        assert!(tree
            .entry_for_path("node_modules/prettier/package.json")
            .is_some_and(|f| f.is_always_included));
    });

    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings::<WorktreeSettings>(cx, |project_settings| {
                project_settings.file_scan_exclusions = Some(vec![]);
                project_settings.file_scan_inclusions = Some(vec![]);
            });
        });
    });
    cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
        .await;
    tree.flush_fs_events(cx).await;

    tree.read_with(cx, |tree, _| {
        assert!(tree
            .entry_for_path("node_modules")
            .is_some_and(|f| !f.is_always_included));
        assert!(tree
            .entry_for_path("node_modules/prettier/package.json")
            .is_some_and(|f| !f.is_always_included));
    });
}

#[gpui::test]
async fn test_file_scan_exclusions(cx: &mut TestAppContext) {
    init_test(cx);
    cx.executor().allow_parking();
    let dir = TempTree::new(json!({
        ".gitignore": "**/target\n/node_modules\n",
        "target": {
            "index": "blah2"
        },
        "node_modules": {
            ".DS_Store": "",
            "prettier": {
                "package.json": "{}",
            },
        },
        "src": {
            ".DS_Store": "",
            "foo": {
                "foo.rs": "mod another;\n",
                "another.rs": "// another",
            },
            "bar": {
                "bar.rs": "// bar",
            },
            "lib.rs": "mod foo;\nmod bar;\n",
        },
        ".DS_Store": "",
    }));
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings::<WorktreeSettings>(cx, |project_settings| {
                project_settings.file_scan_exclusions =
                    Some(vec!["**/foo/**".to_string(), "**/.DS_Store".to_string()]);
            });
        });
    });

    let tree = Worktree::local(
        dir.path(),
        true,
        Arc::new(RealFs::new(None, cx.executor())),
        Default::default(),
        &mut cx.to_async(),
    )
    .await
    .unwrap();
    cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
        .await;
    tree.flush_fs_events(cx).await;
    tree.read_with(cx, |tree, _| {
        check_worktree_entries(
            tree,
            &[
                "src/foo/foo.rs",
                "src/foo/another.rs",
                "node_modules/.DS_Store",
                "src/.DS_Store",
                ".DS_Store",
            ],
            &["target", "node_modules"],
            &["src/lib.rs", "src/bar/bar.rs", ".gitignore"],
            &[],
        )
    });

    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings::<WorktreeSettings>(cx, |project_settings| {
                project_settings.file_scan_exclusions =
                    Some(vec!["**/node_modules/**".to_string()]);
            });
        });
    });
    tree.flush_fs_events(cx).await;
    cx.executor().run_until_parked();
    tree.read_with(cx, |tree, _| {
        check_worktree_entries(
            tree,
            &[
                "node_modules/prettier/package.json",
                "node_modules/.DS_Store",
                "node_modules",
            ],
            &["target"],
            &[
                ".gitignore",
                "src/lib.rs",
                "src/bar/bar.rs",
                "src/foo/foo.rs",
                "src/foo/another.rs",
                "src/.DS_Store",
                ".DS_Store",
            ],
            &[],
        )
    });
}

#[gpui::test]
async fn test_fs_events_in_exclusions(cx: &mut TestAppContext) {
    init_test(cx);
    cx.executor().allow_parking();
    let dir = TempTree::new(json!({
        ".git": {
            "HEAD": "ref: refs/heads/main\n",
            "foo": "bar",
        },
        ".gitignore": "**/target\n/node_modules\ntest_output\n",
        "target": {
            "index": "blah2"
        },
        "node_modules": {
            ".DS_Store": "",
            "prettier": {
                "package.json": "{}",
            },
        },
        "src": {
            ".DS_Store": "",
            "foo": {
                "foo.rs": "mod another;\n",
                "another.rs": "// another",
            },
            "bar": {
                "bar.rs": "// bar",
            },
            "lib.rs": "mod foo;\nmod bar;\n",
        },
        ".DS_Store": "",
    }));
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings::<WorktreeSettings>(cx, |project_settings| {
                project_settings.file_scan_exclusions = Some(vec![
                    "**/.git".to_string(),
                    "node_modules/".to_string(),
                    "build_output".to_string(),
                ]);
            });
        });
    });

    let tree = Worktree::local(
        dir.path(),
        true,
        Arc::new(RealFs::new(None, cx.executor())),
        Default::default(),
        &mut cx.to_async(),
    )
    .await
    .unwrap();
    cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
        .await;
    tree.flush_fs_events(cx).await;
    tree.read_with(cx, |tree, _| {
        check_worktree_entries(
            tree,
            &[
                ".git/HEAD",
                ".git/foo",
                "node_modules",
                "node_modules/.DS_Store",
                "node_modules/prettier",
                "node_modules/prettier/package.json",
            ],
            &["target"],
            &[
                ".DS_Store",
                "src/.DS_Store",
                "src/lib.rs",
                "src/foo/foo.rs",
                "src/foo/another.rs",
                "src/bar/bar.rs",
                ".gitignore",
            ],
            &[],
        )
    });

    let new_excluded_dir = dir.path().join("build_output");
    let new_ignored_dir = dir.path().join("test_output");
    std::fs::create_dir_all(&new_excluded_dir)
        .unwrap_or_else(|e| panic!("Failed to create a {new_excluded_dir:?} directory: {e}"));
    std::fs::create_dir_all(&new_ignored_dir)
        .unwrap_or_else(|e| panic!("Failed to create a {new_ignored_dir:?} directory: {e}"));
    let node_modules_dir = dir.path().join("node_modules");
    let dot_git_dir = dir.path().join(".git");
    let src_dir = dir.path().join("src");
    for existing_dir in [&node_modules_dir, &dot_git_dir, &src_dir] {
        assert!(
            existing_dir.is_dir(),
            "Expect {existing_dir:?} to be present in the FS already"
        );
    }

    for directory_for_new_file in [
        new_excluded_dir,
        new_ignored_dir,
        node_modules_dir,
        dot_git_dir,
        src_dir,
    ] {
        std::fs::write(directory_for_new_file.join("new_file"), "new file contents")
            .unwrap_or_else(|e| {
                panic!("Failed to create in {directory_for_new_file:?} a new file: {e}")
            });
    }
    tree.flush_fs_events(cx).await;

    tree.read_with(cx, |tree, _| {
        check_worktree_entries(
            tree,
            &[
                ".git/HEAD",
                ".git/foo",
                ".git/new_file",
                "node_modules",
                "node_modules/.DS_Store",
                "node_modules/prettier",
                "node_modules/prettier/package.json",
                "node_modules/new_file",
                "build_output",
                "build_output/new_file",
                "test_output/new_file",
            ],
            &["target", "test_output"],
            &[
                ".DS_Store",
                "src/.DS_Store",
                "src/lib.rs",
                "src/foo/foo.rs",
                "src/foo/another.rs",
                "src/bar/bar.rs",
                "src/new_file",
                ".gitignore",
            ],
            &[],
        )
    });
}

#[gpui::test]
async fn test_fs_events_in_dot_git_worktree(cx: &mut TestAppContext) {
    init_test(cx);
    cx.executor().allow_parking();
    let dir = TempTree::new(json!({
        ".git": {
            "HEAD": "ref: refs/heads/main\n",
            "foo": "foo contents",
        },
    }));
    let dot_git_worktree_dir = dir.path().join(".git");

    let tree = Worktree::local(
        dot_git_worktree_dir.clone(),
        true,
        Arc::new(RealFs::new(None, cx.executor())),
        Default::default(),
        &mut cx.to_async(),
    )
    .await
    .unwrap();
    cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
        .await;
    tree.flush_fs_events(cx).await;
    tree.read_with(cx, |tree, _| {
        check_worktree_entries(tree, &[], &["HEAD", "foo"], &[], &[])
    });

    std::fs::write(dot_git_worktree_dir.join("new_file"), "new file contents")
        .unwrap_or_else(|e| panic!("Failed to create in {dot_git_worktree_dir:?} a new file: {e}"));
    tree.flush_fs_events(cx).await;
    tree.read_with(cx, |tree, _| {
        check_worktree_entries(tree, &[], &["HEAD", "foo", "new_file"], &[], &[])
    });
}

#[gpui::test(iterations = 30)]
async fn test_create_directory_during_initial_scan(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.background_executor.clone());
    fs.insert_tree(
        "/root",
        json!({
            "b": {},
            "c": {},
            "d": {},
        }),
    )
    .await;

    let tree = Worktree::local(
        "/root".as_ref(),
        true,
        fs,
        Default::default(),
        &mut cx.to_async(),
    )
    .await
    .unwrap();

    let snapshot1 = tree.update(cx, |tree, cx| {
        let tree = tree.as_local_mut().unwrap();
        let snapshot = Arc::new(Mutex::new(tree.snapshot()));
        tree.observe_updates(0, cx, {
            let snapshot = snapshot.clone();
            let settings = tree.settings().clone();
            move |update| {
                snapshot
                    .lock()
                    .apply_remote_update(update, &settings.file_scan_inclusions)
                    .unwrap();
                async { true }
            }
        });
        snapshot
    });

    let entry = tree
        .update(cx, |tree, cx| {
            tree.as_local_mut()
                .unwrap()
                .create_entry("a/e".as_ref(), true, cx)
        })
        .await
        .unwrap()
        .to_included()
        .unwrap();
    assert!(entry.is_dir());

    cx.executor().run_until_parked();
    tree.read_with(cx, |tree, _| {
        assert_eq!(tree.entry_for_path("a/e").unwrap().kind, EntryKind::Dir);
    });

    let snapshot2 = tree.update(cx, |tree, _| tree.as_local().unwrap().snapshot());
    assert_eq!(
        snapshot1.lock().entries(true, 0).collect::<Vec<_>>(),
        snapshot2.entries(true, 0).collect::<Vec<_>>()
    );
}

#[gpui::test]
async fn test_create_dir_all_on_create_entry(cx: &mut TestAppContext) {
    init_test(cx);
    cx.executor().allow_parking();

    let fs_fake = FakeFs::new(cx.background_executor.clone());
    fs_fake
        .insert_tree(
            "/root",
            json!({
                "a": {},
            }),
        )
        .await;

    let tree_fake = Worktree::local(
        "/root".as_ref(),
        true,
        fs_fake,
        Default::default(),
        &mut cx.to_async(),
    )
    .await
    .unwrap();

    let entry = tree_fake
        .update(cx, |tree, cx| {
            tree.as_local_mut()
                .unwrap()
                .create_entry("a/b/c/d.txt".as_ref(), false, cx)
        })
        .await
        .unwrap()
        .to_included()
        .unwrap();
    assert!(entry.is_file());

    cx.executor().run_until_parked();
    tree_fake.read_with(cx, |tree, _| {
        assert!(tree.entry_for_path("a/b/c/d.txt").unwrap().is_file());
        assert!(tree.entry_for_path("a/b/c/").unwrap().is_dir());
        assert!(tree.entry_for_path("a/b/").unwrap().is_dir());
    });

    let fs_real = Arc::new(RealFs::new(None, cx.executor()));
    let temp_root = TempTree::new(json!({
        "a": {}
    }));

    let tree_real = Worktree::local(
        temp_root.path(),
        true,
        fs_real,
        Default::default(),
        &mut cx.to_async(),
    )
    .await
    .unwrap();

    let entry = tree_real
        .update(cx, |tree, cx| {
            tree.as_local_mut()
                .unwrap()
                .create_entry("a/b/c/d.txt".as_ref(), false, cx)
        })
        .await
        .unwrap()
        .to_included()
        .unwrap();
    assert!(entry.is_file());

    cx.executor().run_until_parked();
    tree_real.read_with(cx, |tree, _| {
        assert!(tree.entry_for_path("a/b/c/d.txt").unwrap().is_file());
        assert!(tree.entry_for_path("a/b/c/").unwrap().is_dir());
        assert!(tree.entry_for_path("a/b/").unwrap().is_dir());
    });

    // Test smallest change
    let entry = tree_real
        .update(cx, |tree, cx| {
            tree.as_local_mut()
                .unwrap()
                .create_entry("a/b/c/e.txt".as_ref(), false, cx)
        })
        .await
        .unwrap()
        .to_included()
        .unwrap();
    assert!(entry.is_file());

    cx.executor().run_until_parked();
    tree_real.read_with(cx, |tree, _| {
        assert!(tree.entry_for_path("a/b/c/e.txt").unwrap().is_file());
    });

    // Test largest change
    let entry = tree_real
        .update(cx, |tree, cx| {
            tree.as_local_mut()
                .unwrap()
                .create_entry("d/e/f/g.txt".as_ref(), false, cx)
        })
        .await
        .unwrap()
        .to_included()
        .unwrap();
    assert!(entry.is_file());

    cx.executor().run_until_parked();
    tree_real.read_with(cx, |tree, _| {
        assert!(tree.entry_for_path("d/e/f/g.txt").unwrap().is_file());
        assert!(tree.entry_for_path("d/e/f").unwrap().is_dir());
        assert!(tree.entry_for_path("d/e/").unwrap().is_dir());
        assert!(tree.entry_for_path("d/").unwrap().is_dir());
    });
}

#[gpui::test(iterations = 100)]
async fn test_random_worktree_operations_during_initial_scan(
    cx: &mut TestAppContext,
    mut rng: StdRng,
) {
    init_test(cx);
    let operations = env::var("OPERATIONS")
        .map(|o| o.parse().unwrap())
        .unwrap_or(5);
    let initial_entries = env::var("INITIAL_ENTRIES")
        .map(|o| o.parse().unwrap())
        .unwrap_or(20);

    let root_dir = Path::new(path!("/test"));
    let fs = FakeFs::new(cx.background_executor.clone()) as Arc<dyn Fs>;
    fs.as_fake().insert_tree(root_dir, json!({})).await;
    for _ in 0..initial_entries {
        randomly_mutate_fs(&fs, root_dir, 1.0, &mut rng).await;
    }
    log::info!("generated initial tree");

    let worktree = Worktree::local(
        root_dir,
        true,
        fs.clone(),
        Default::default(),
        &mut cx.to_async(),
    )
    .await
    .unwrap();

    let mut snapshots = vec![worktree.read_with(cx, |tree, _| tree.as_local().unwrap().snapshot())];
    let updates = Arc::new(Mutex::new(Vec::new()));
    worktree.update(cx, |tree, cx| {
        check_worktree_change_events(tree, cx);

        tree.as_local_mut().unwrap().observe_updates(0, cx, {
            let updates = updates.clone();
            move |update| {
                updates.lock().push(update);
                async { true }
            }
        });
    });

    for _ in 0..operations {
        worktree
            .update(cx, |worktree, cx| {
                randomly_mutate_worktree(worktree, &mut rng, cx)
            })
            .await
            .log_err();
        worktree.read_with(cx, |tree, _| {
            tree.as_local().unwrap().snapshot().check_invariants(true)
        });

        if rng.gen_bool(0.6) {
            snapshots.push(worktree.read_with(cx, |tree, _| tree.as_local().unwrap().snapshot()));
        }
    }

    worktree
        .update(cx, |tree, _| tree.as_local_mut().unwrap().scan_complete())
        .await;

    cx.executor().run_until_parked();

    let final_snapshot = worktree.read_with(cx, |tree, _| {
        let tree = tree.as_local().unwrap();
        let snapshot = tree.snapshot();
        snapshot.check_invariants(true);
        snapshot
    });

    let settings = worktree.read_with(cx, |tree, _| tree.as_local().unwrap().settings());

    for (i, snapshot) in snapshots.into_iter().enumerate().rev() {
        let mut updated_snapshot = snapshot.clone();
        for update in updates.lock().iter() {
            if update.scan_id >= updated_snapshot.scan_id() as u64 {
                updated_snapshot
                    .apply_remote_update(update.clone(), &settings.file_scan_inclusions)
                    .unwrap();
            }
        }

        assert_eq!(
            updated_snapshot.entries(true, 0).collect::<Vec<_>>(),
            final_snapshot.entries(true, 0).collect::<Vec<_>>(),
            "wrong updates after snapshot {i}: {snapshot:#?} {updates:#?}",
        );
    }
}

#[gpui::test(iterations = 100)]
async fn test_random_worktree_changes(cx: &mut TestAppContext, mut rng: StdRng) {
    init_test(cx);
    let operations = env::var("OPERATIONS")
        .map(|o| o.parse().unwrap())
        .unwrap_or(40);
    let initial_entries = env::var("INITIAL_ENTRIES")
        .map(|o| o.parse().unwrap())
        .unwrap_or(20);

    let root_dir = Path::new(path!("/test"));
    let fs = FakeFs::new(cx.background_executor.clone()) as Arc<dyn Fs>;
    fs.as_fake().insert_tree(root_dir, json!({})).await;
    for _ in 0..initial_entries {
        randomly_mutate_fs(&fs, root_dir, 1.0, &mut rng).await;
    }
    log::info!("generated initial tree");

    let worktree = Worktree::local(
        root_dir,
        true,
        fs.clone(),
        Default::default(),
        &mut cx.to_async(),
    )
    .await
    .unwrap();

    let updates = Arc::new(Mutex::new(Vec::new()));
    worktree.update(cx, |tree, cx| {
        check_worktree_change_events(tree, cx);

        tree.as_local_mut().unwrap().observe_updates(0, cx, {
            let updates = updates.clone();
            move |update| {
                updates.lock().push(update);
                async { true }
            }
        });
    });

    worktree
        .update(cx, |tree, _| tree.as_local_mut().unwrap().scan_complete())
        .await;

    fs.as_fake().pause_events();
    let mut snapshots = Vec::new();
    let mut mutations_len = operations;
    while mutations_len > 1 {
        if rng.gen_bool(0.2) {
            worktree
                .update(cx, |worktree, cx| {
                    randomly_mutate_worktree(worktree, &mut rng, cx)
                })
                .await
                .log_err();
        } else {
            randomly_mutate_fs(&fs, root_dir, 1.0, &mut rng).await;
        }

        let buffered_event_count = fs.as_fake().buffered_event_count();
        if buffered_event_count > 0 && rng.gen_bool(0.3) {
            let len = rng.gen_range(0..=buffered_event_count);
            log::info!("flushing {} events", len);
            fs.as_fake().flush_events(len);
        } else {
            randomly_mutate_fs(&fs, root_dir, 0.6, &mut rng).await;
            mutations_len -= 1;
        }

        cx.executor().run_until_parked();
        if rng.gen_bool(0.2) {
            log::info!("storing snapshot {}", snapshots.len());
            let snapshot = worktree.read_with(cx, |tree, _| tree.as_local().unwrap().snapshot());
            snapshots.push(snapshot);
        }
    }

    log::info!("quiescing");
    fs.as_fake().flush_events(usize::MAX);
    cx.executor().run_until_parked();

    let snapshot = worktree.read_with(cx, |tree, _| tree.as_local().unwrap().snapshot());
    snapshot.check_invariants(true);
    let expanded_paths = snapshot
        .expanded_entries()
        .map(|e| e.path.clone())
        .collect::<Vec<_>>();

    {
        let new_worktree = Worktree::local(
            root_dir,
            true,
            fs.clone(),
            Default::default(),
            &mut cx.to_async(),
        )
        .await
        .unwrap();
        new_worktree
            .update(cx, |tree, _| tree.as_local_mut().unwrap().scan_complete())
            .await;
        new_worktree
            .update(cx, |tree, _| {
                tree.as_local_mut()
                    .unwrap()
                    .refresh_entries_for_paths(expanded_paths)
            })
            .recv()
            .await;
        let new_snapshot =
            new_worktree.read_with(cx, |tree, _| tree.as_local().unwrap().snapshot());
        assert_eq!(
            snapshot.entries_without_ids(true),
            new_snapshot.entries_without_ids(true)
        );
    }

    let settings = worktree.read_with(cx, |tree, _| tree.as_local().unwrap().settings());

    for (i, mut prev_snapshot) in snapshots.into_iter().enumerate().rev() {
        for update in updates.lock().iter() {
            if update.scan_id >= prev_snapshot.scan_id() as u64 {
                prev_snapshot
                    .apply_remote_update(update.clone(), &settings.file_scan_inclusions)
                    .unwrap();
            }
        }

        assert_eq!(
            prev_snapshot
                .entries(true, 0)
                .map(ignore_pending_dir)
                .collect::<Vec<_>>(),
            snapshot
                .entries(true, 0)
                .map(ignore_pending_dir)
                .collect::<Vec<_>>(),
            "wrong updates after snapshot {i}: {updates:#?}",
        );
    }

    fn ignore_pending_dir(entry: &Entry) -> Entry {
        let mut entry = entry.clone();
        if entry.kind.is_dir() {
            entry.kind = EntryKind::Dir
        }
        entry
    }
}

// The worktree's `UpdatedEntries` event can be used to follow along with
// all changes to the worktree's snapshot.
fn check_worktree_change_events(tree: &mut Worktree, cx: &mut Context<Worktree>) {
    let mut entries = tree.entries(true, 0).cloned().collect::<Vec<_>>();
    cx.subscribe(&cx.entity(), move |tree, _, event, _| {
        if let Event::UpdatedEntries(changes) = event {
            for (path, _, change_type) in changes.iter() {
                let entry = tree.entry_for_path(path).cloned();
                let ix = match entries.binary_search_by_key(&path, |e| &e.path) {
                    Ok(ix) | Err(ix) => ix,
                };
                match change_type {
                    PathChange::Added => entries.insert(ix, entry.unwrap()),
                    PathChange::Removed => drop(entries.remove(ix)),
                    PathChange::Updated => {
                        let entry = entry.unwrap();
                        let existing_entry = entries.get_mut(ix).unwrap();
                        assert_eq!(existing_entry.path, entry.path);
                        *existing_entry = entry;
                    }
                    PathChange::AddedOrUpdated | PathChange::Loaded => {
                        let entry = entry.unwrap();
                        if entries.get(ix).map(|e| &e.path) == Some(&entry.path) {
                            *entries.get_mut(ix).unwrap() = entry;
                        } else {
                            entries.insert(ix, entry);
                        }
                    }
                }
            }

            let new_entries = tree.entries(true, 0).cloned().collect::<Vec<_>>();
            assert_eq!(entries, new_entries, "incorrect changes: {:?}", changes);
        }
    })
    .detach();
}

fn randomly_mutate_worktree(
    worktree: &mut Worktree,
    rng: &mut impl Rng,
    cx: &mut Context<Worktree>,
) -> Task<Result<()>> {
    log::info!("mutating worktree");
    let worktree = worktree.as_local_mut().unwrap();
    let snapshot = worktree.snapshot();
    let entry = snapshot.entries(false, 0).choose(rng).unwrap();

    match rng.gen_range(0_u32..100) {
        0..=33 if entry.path.as_ref() != Path::new("") => {
            log::info!("deleting entry {:?} ({})", entry.path, entry.id.0);
            worktree.delete_entry(entry.id, false, cx).unwrap()
        }
        ..=66 if entry.path.as_ref() != Path::new("") => {
            let other_entry = snapshot.entries(false, 0).choose(rng).unwrap();
            let new_parent_path = if other_entry.is_dir() {
                other_entry.path.clone()
            } else {
                other_entry.path.parent().unwrap().into()
            };
            let mut new_path = new_parent_path.join(random_filename(rng));
            if new_path.starts_with(&entry.path) {
                new_path = random_filename(rng).into();
            }

            log::info!(
                "renaming entry {:?} ({}) to {:?}",
                entry.path,
                entry.id.0,
                new_path
            );
            let task = worktree.rename_entry(entry.id, new_path, cx);
            cx.background_spawn(async move {
                task.await?.to_included().unwrap();
                Ok(())
            })
        }
        _ => {
            if entry.is_dir() {
                let child_path = entry.path.join(random_filename(rng));
                let is_dir = rng.gen_bool(0.3);
                log::info!(
                    "creating {} at {:?}",
                    if is_dir { "dir" } else { "file" },
                    child_path,
                );
                let task = worktree.create_entry(child_path, is_dir, cx);
                cx.background_spawn(async move {
                    task.await?;
                    Ok(())
                })
            } else {
                log::info!("overwriting file {:?} ({})", entry.path, entry.id.0);
                let task =
                    worktree.write_file(entry.path.clone(), "".into(), Default::default(), cx);
                cx.background_spawn(async move {
                    task.await?;
                    Ok(())
                })
            }
        }
    }
}

async fn randomly_mutate_fs(
    fs: &Arc<dyn Fs>,
    root_path: &Path,
    insertion_probability: f64,
    rng: &mut impl Rng,
) {
    log::info!("mutating fs");
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    for path in fs.as_fake().paths(false) {
        if path.starts_with(root_path) {
            if fs.is_file(&path).await {
                files.push(path);
            } else {
                dirs.push(path);
            }
        }
    }

    if (files.is_empty() && dirs.len() == 1) || rng.gen_bool(insertion_probability) {
        let path = dirs.choose(rng).unwrap();
        let new_path = path.join(random_filename(rng));

        if rng.gen() {
            log::info!(
                "creating dir {:?}",
                new_path.strip_prefix(root_path).unwrap()
            );
            fs.create_dir(&new_path).await.unwrap();
        } else {
            log::info!(
                "creating file {:?}",
                new_path.strip_prefix(root_path).unwrap()
            );
            fs.create_file(&new_path, Default::default()).await.unwrap();
        }
    } else if rng.gen_bool(0.05) {
        let ignore_dir_path = dirs.choose(rng).unwrap();
        let ignore_path = ignore_dir_path.join(*GITIGNORE);

        let subdirs = dirs
            .iter()
            .filter(|d| d.starts_with(ignore_dir_path))
            .cloned()
            .collect::<Vec<_>>();
        let subfiles = files
            .iter()
            .filter(|d| d.starts_with(ignore_dir_path))
            .cloned()
            .collect::<Vec<_>>();
        let files_to_ignore = {
            let len = rng.gen_range(0..=subfiles.len());
            subfiles.choose_multiple(rng, len)
        };
        let dirs_to_ignore = {
            let len = rng.gen_range(0..subdirs.len());
            subdirs.choose_multiple(rng, len)
        };

        let mut ignore_contents = String::new();
        for path_to_ignore in files_to_ignore.chain(dirs_to_ignore) {
            writeln!(
                ignore_contents,
                "{}",
                path_to_ignore
                    .strip_prefix(ignore_dir_path)
                    .unwrap()
                    .to_str()
                    .unwrap()
            )
            .unwrap();
        }
        log::info!(
            "creating gitignore {:?} with contents:\n{}",
            ignore_path.strip_prefix(root_path).unwrap(),
            ignore_contents
        );
        fs.save(
            &ignore_path,
            &ignore_contents.as_str().into(),
            Default::default(),
        )
        .await
        .unwrap();
    } else {
        let old_path = {
            let file_path = files.choose(rng);
            let dir_path = dirs[1..].choose(rng);
            file_path.into_iter().chain(dir_path).choose(rng).unwrap()
        };

        let is_rename = rng.gen();
        if is_rename {
            let new_path_parent = dirs
                .iter()
                .filter(|d| !d.starts_with(old_path))
                .choose(rng)
                .unwrap();

            let overwrite_existing_dir =
                !old_path.starts_with(new_path_parent) && rng.gen_bool(0.3);
            let new_path = if overwrite_existing_dir {
                fs.remove_dir(
                    new_path_parent,
                    RemoveOptions {
                        recursive: true,
                        ignore_if_not_exists: true,
                    },
                )
                .await
                .unwrap();
                new_path_parent.to_path_buf()
            } else {
                new_path_parent.join(random_filename(rng))
            };

            log::info!(
                "renaming {:?} to {}{:?}",
                old_path.strip_prefix(root_path).unwrap(),
                if overwrite_existing_dir {
                    "overwrite "
                } else {
                    ""
                },
                new_path.strip_prefix(root_path).unwrap()
            );
            fs.rename(
                old_path,
                &new_path,
                fs::RenameOptions {
                    overwrite: true,
                    ignore_if_exists: true,
                },
            )
            .await
            .unwrap();
        } else if fs.is_file(old_path).await {
            log::info!(
                "deleting file {:?}",
                old_path.strip_prefix(root_path).unwrap()
            );
            fs.remove_file(old_path, Default::default()).await.unwrap();
        } else {
            log::info!(
                "deleting dir {:?}",
                old_path.strip_prefix(root_path).unwrap()
            );
            fs.remove_dir(
                old_path,
                RemoveOptions {
                    recursive: true,
                    ignore_if_not_exists: true,
                },
            )
            .await
            .unwrap();
        }
    }
}

fn random_filename(rng: &mut impl Rng) -> String {
    (0..6)
        .map(|_| rng.sample(rand::distributions::Alphanumeric))
        .map(char::from)
        .collect()
}

// NOTE:
// This test always fails on Windows, because on Windows, unlike on Unix, you can't rename
// a directory which some program has already open.
// This is a limitation of the Windows.
// See: https://stackoverflow.com/questions/41365318/access-is-denied-when-renaming-folder
#[gpui::test]
#[cfg_attr(target_os = "windows", ignore)]
async fn test_rename_work_directory(cx: &mut TestAppContext) {
    init_test(cx);
    cx.executor().allow_parking();
    let root = TempTree::new(json!({
        "projects": {
            "project1": {
                "a": "",
                "b": "",
            }
        },

    }));
    let root_path = root.path();

    let tree = Worktree::local(
        root_path,
        true,
        Arc::new(RealFs::new(None, cx.executor())),
        Default::default(),
        &mut cx.to_async(),
    )
    .await
    .unwrap();

    let repo = git_init(&root_path.join("projects/project1"));
    git_add("a", &repo);
    git_commit("init", &repo);
    std::fs::write(root_path.join("projects/project1/a"), "aa").unwrap();

    cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
        .await;

    tree.flush_fs_events(cx).await;

    cx.read(|cx| {
        let tree = tree.read(cx);
        let repo = tree.repositories.iter().next().unwrap();
        assert_eq!(
            repo.work_directory_abs_path,
            root_path.join("projects/project1")
        );
        assert_eq!(
            repo.status_for_path(&"a".into()).map(|entry| entry.status),
            Some(StatusCode::Modified.worktree()),
        );
        assert_eq!(
            repo.status_for_path(&"b".into()).map(|entry| entry.status),
            Some(FileStatus::Untracked),
        );
    });

    std::fs::rename(
        root_path.join("projects/project1"),
        root_path.join("projects/project2"),
    )
    .unwrap();
    tree.flush_fs_events(cx).await;

    cx.read(|cx| {
        let tree = tree.read(cx);
        let repo = tree.repositories.iter().next().unwrap();
        assert_eq!(
            repo.work_directory_abs_path,
            root_path.join("projects/project2")
        );
        assert_eq!(
            repo.status_for_path(&"a".into()).unwrap().status,
            StatusCode::Modified.worktree(),
        );
        assert_eq!(
            repo.status_for_path(&"b".into()).unwrap().status,
            FileStatus::Untracked,
        );
    });
}

// NOTE: This test always fails on Windows, because on Windows, unlike on Unix,
// you can't rename a directory which some program has already open. This is a
// limitation of the Windows. See:
// https://stackoverflow.com/questions/41365318/access-is-denied-when-renaming-folder
#[gpui::test]
#[cfg_attr(target_os = "windows", ignore)]
async fn test_file_status(cx: &mut TestAppContext) {
    init_test(cx);
    cx.executor().allow_parking();
    const IGNORE_RULE: &str = "**/target";

    let root = TempTree::new(json!({
        "project": {
            "a.txt": "a",
            "b.txt": "bb",
            "c": {
                "d": {
                    "e.txt": "eee"
                }
            },
            "f.txt": "ffff",
            "target": {
                "build_file": "???"
            },
            ".gitignore": IGNORE_RULE
        },

    }));

    const A_TXT: &str = "a.txt";
    const B_TXT: &str = "b.txt";
    const E_TXT: &str = "c/d/e.txt";
    const F_TXT: &str = "f.txt";
    const DOTGITIGNORE: &str = ".gitignore";
    const BUILD_FILE: &str = "target/build_file";

    // Set up git repository before creating the worktree.
    let work_dir = root.path().join("project");
    let mut repo = git_init(work_dir.as_path());
    repo.add_ignore_rule(IGNORE_RULE).unwrap();
    git_add(A_TXT, &repo);
    git_add(E_TXT, &repo);
    git_add(DOTGITIGNORE, &repo);
    git_commit("Initial commit", &repo);

    let tree = Worktree::local(
        root.path(),
        true,
        Arc::new(RealFs::new(None, cx.executor())),
        Default::default(),
        &mut cx.to_async(),
    )
    .await
    .unwrap();
    let root_path = root.path();

    tree.flush_fs_events(cx).await;
    cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
        .await;
    cx.executor().run_until_parked();

    // Check that the right git state is observed on startup
    tree.read_with(cx, |tree, _cx| {
        let snapshot = tree.snapshot();
        assert_eq!(snapshot.repositories.iter().count(), 1);
        let repo_entry = snapshot.repositories.iter().next().unwrap();
        assert_eq!(
            repo_entry.work_directory_abs_path,
            root_path.join("project")
        );

        assert_eq!(
            repo_entry.status_for_path(&B_TXT.into()).unwrap().status,
            FileStatus::Untracked,
        );
        assert_eq!(
            repo_entry.status_for_path(&F_TXT.into()).unwrap().status,
            FileStatus::Untracked,
        );
    });

    // Modify a file in the working copy.
    std::fs::write(work_dir.join(A_TXT), "aa").unwrap();
    tree.flush_fs_events(cx).await;
    cx.executor().run_until_parked();

    // The worktree detects that the file's git status has changed.
    tree.read_with(cx, |tree, _cx| {
        let snapshot = tree.snapshot();
        assert_eq!(snapshot.repositories.iter().count(), 1);
        let repo_entry = snapshot.repositories.iter().next().unwrap();
        assert_eq!(
            repo_entry.status_for_path(&A_TXT.into()).unwrap().status,
            StatusCode::Modified.worktree(),
        );
    });

    // Create a commit in the git repository.
    git_add(A_TXT, &repo);
    git_add(B_TXT, &repo);
    git_commit("Committing modified and added", &repo);
    tree.flush_fs_events(cx).await;
    cx.executor().run_until_parked();

    // The worktree detects that the files' git status have changed.
    tree.read_with(cx, |tree, _cx| {
        let snapshot = tree.snapshot();
        assert_eq!(snapshot.repositories.iter().count(), 1);
        let repo_entry = snapshot.repositories.iter().next().unwrap();
        assert_eq!(
            repo_entry.status_for_path(&F_TXT.into()).unwrap().status,
            FileStatus::Untracked,
        );
        assert_eq!(repo_entry.status_for_path(&B_TXT.into()), None);
        assert_eq!(repo_entry.status_for_path(&A_TXT.into()), None);
    });

    // Modify files in the working copy and perform git operations on other files.
    git_reset(0, &repo);
    git_remove_index(Path::new(B_TXT), &repo);
    git_stash(&mut repo);
    std::fs::write(work_dir.join(E_TXT), "eeee").unwrap();
    std::fs::write(work_dir.join(BUILD_FILE), "this should be ignored").unwrap();
    tree.flush_fs_events(cx).await;
    cx.executor().run_until_parked();

    // Check that more complex repo changes are tracked
    tree.read_with(cx, |tree, _cx| {
        let snapshot = tree.snapshot();
        assert_eq!(snapshot.repositories.iter().count(), 1);
        let repo_entry = snapshot.repositories.iter().next().unwrap();

        assert_eq!(repo_entry.status_for_path(&A_TXT.into()), None);
        assert_eq!(
            repo_entry.status_for_path(&B_TXT.into()).unwrap().status,
            FileStatus::Untracked,
        );
        assert_eq!(
            repo_entry.status_for_path(&E_TXT.into()).unwrap().status,
            StatusCode::Modified.worktree(),
        );
    });

    std::fs::remove_file(work_dir.join(B_TXT)).unwrap();
    std::fs::remove_dir_all(work_dir.join("c")).unwrap();
    std::fs::write(
        work_dir.join(DOTGITIGNORE),
        [IGNORE_RULE, "f.txt"].join("\n"),
    )
    .unwrap();

    git_add(Path::new(DOTGITIGNORE), &repo);
    git_commit("Committing modified git ignore", &repo);

    tree.flush_fs_events(cx).await;
    cx.executor().run_until_parked();

    let mut renamed_dir_name = "first_directory/second_directory";
    const RENAMED_FILE: &str = "rf.txt";

    std::fs::create_dir_all(work_dir.join(renamed_dir_name)).unwrap();
    std::fs::write(
        work_dir.join(renamed_dir_name).join(RENAMED_FILE),
        "new-contents",
    )
    .unwrap();

    tree.flush_fs_events(cx).await;
    cx.executor().run_until_parked();

    tree.read_with(cx, |tree, _cx| {
        let snapshot = tree.snapshot();
        assert_eq!(snapshot.repositories.iter().count(), 1);
        let repo_entry = snapshot.repositories.iter().next().unwrap();
        assert_eq!(
            repo_entry
                .status_for_path(&Path::new(renamed_dir_name).join(RENAMED_FILE).into())
                .unwrap()
                .status,
            FileStatus::Untracked,
        );
    });

    renamed_dir_name = "new_first_directory/second_directory";

    std::fs::rename(
        work_dir.join("first_directory"),
        work_dir.join("new_first_directory"),
    )
    .unwrap();

    tree.flush_fs_events(cx).await;
    cx.executor().run_until_parked();

    tree.read_with(cx, |tree, _cx| {
        let snapshot = tree.snapshot();
        assert_eq!(snapshot.repositories.iter().count(), 1);
        let repo_entry = snapshot.repositories.iter().next().unwrap();

        assert_eq!(
            repo_entry
                .status_for_path(&Path::new(renamed_dir_name).join(RENAMED_FILE).into())
                .unwrap()
                .status,
            FileStatus::Untracked,
        );
    });
}

#[gpui::test]
async fn test_git_repository_status(cx: &mut TestAppContext) {
    init_test(cx);
    cx.executor().allow_parking();

    let root = TempTree::new(json!({
        "project": {
            "a.txt": "a",    // Modified
            "b.txt": "bb",   // Added
            "c.txt": "ccc",  // Unchanged
            "d.txt": "dddd", // Deleted
        },

    }));

    // Set up git repository before creating the worktree.
    let work_dir = root.path().join("project");
    let repo = git_init(work_dir.as_path());
    git_add("a.txt", &repo);
    git_add("c.txt", &repo);
    git_add("d.txt", &repo);
    git_commit("Initial commit", &repo);
    std::fs::remove_file(work_dir.join("d.txt")).unwrap();
    std::fs::write(work_dir.join("a.txt"), "aa").unwrap();

    let tree = Worktree::local(
        root.path(),
        true,
        Arc::new(RealFs::new(None, cx.executor())),
        Default::default(),
        &mut cx.to_async(),
    )
    .await
    .unwrap();

    tree.flush_fs_events(cx).await;
    cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
        .await;
    cx.executor().run_until_parked();

    // Check that the right git state is observed on startup
    tree.read_with(cx, |tree, _cx| {
        let snapshot = tree.snapshot();
        let repo = snapshot.repositories.iter().next().unwrap();
        let entries = repo.status().collect::<Vec<_>>();

        assert_eq!(
            entries,
            [
                StatusEntry {
                    repo_path: "a.txt".into(),
                    status: StatusCode::Modified.worktree(),
                },
                StatusEntry {
                    repo_path: "b.txt".into(),
                    status: FileStatus::Untracked,
                },
                StatusEntry {
                    repo_path: "d.txt".into(),
                    status: StatusCode::Deleted.worktree(),
                },
            ]
        );
    });

    std::fs::write(work_dir.join("c.txt"), "some changes").unwrap();

    tree.flush_fs_events(cx).await;
    cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
        .await;
    cx.executor().run_until_parked();

    tree.read_with(cx, |tree, _cx| {
        let snapshot = tree.snapshot();
        let repository = snapshot.repositories.iter().next().unwrap();
        let entries = repository.status().collect::<Vec<_>>();

        assert_eq!(
            entries,
            [
                StatusEntry {
                    repo_path: "a.txt".into(),
                    status: StatusCode::Modified.worktree(),
                },
                StatusEntry {
                    repo_path: "b.txt".into(),
                    status: FileStatus::Untracked,
                },
                StatusEntry {
                    repo_path: "c.txt".into(),
                    status: StatusCode::Modified.worktree(),
                },
                StatusEntry {
                    repo_path: "d.txt".into(),
                    status: StatusCode::Deleted.worktree(),
                },
            ]
        );
    });

    git_add("a.txt", &repo);
    git_add("c.txt", &repo);
    git_remove_index(Path::new("d.txt"), &repo);
    git_commit("Another commit", &repo);
    tree.flush_fs_events(cx).await;
    cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
        .await;
    cx.executor().run_until_parked();

    std::fs::remove_file(work_dir.join("a.txt")).unwrap();
    std::fs::remove_file(work_dir.join("b.txt")).unwrap();
    tree.flush_fs_events(cx).await;
    cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
        .await;
    cx.executor().run_until_parked();

    tree.read_with(cx, |tree, _cx| {
        let snapshot = tree.snapshot();
        let repo = snapshot.repositories.iter().next().unwrap();
        let entries = repo.status().collect::<Vec<_>>();

        // Deleting an untracked entry, b.txt, should leave no status
        // a.txt was tracked, and so should have a status
        assert_eq!(
            entries,
            [StatusEntry {
                repo_path: "a.txt".into(),
                status: StatusCode::Deleted.worktree(),
            }]
        );
    });
}

#[gpui::test]
async fn test_git_status_postprocessing(cx: &mut TestAppContext) {
    init_test(cx);
    cx.executor().allow_parking();

    let root = TempTree::new(json!({
        "project": {
            "sub": {},
            "a.txt": "",
        },
    }));

    let work_dir = root.path().join("project");
    let repo = git_init(work_dir.as_path());
    // a.txt exists in HEAD and the working copy but is deleted in the index.
    git_add("a.txt", &repo);
    git_commit("Initial commit", &repo);
    git_remove_index("a.txt".as_ref(), &repo);
    // `sub` is a nested git repository.
    let _sub = git_init(&work_dir.join("sub"));

    let tree = Worktree::local(
        root.path(),
        true,
        Arc::new(RealFs::new(None, cx.executor())),
        Default::default(),
        &mut cx.to_async(),
    )
    .await
    .unwrap();

    tree.flush_fs_events(cx).await;
    cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
        .await;
    cx.executor().run_until_parked();

    tree.read_with(cx, |tree, _cx| {
        let snapshot = tree.snapshot();
        let repo = snapshot.repositories.iter().next().unwrap();
        let entries = repo.status().collect::<Vec<_>>();

        // `sub` doesn't appear in our computed statuses.
        // a.txt appears with a combined `DA` status.
        assert_eq!(
            entries,
            [StatusEntry {
                repo_path: "a.txt".into(),
                status: TrackedStatus {
                    index_status: StatusCode::Deleted,
                    worktree_status: StatusCode::Added
                }
                .into(),
            }]
        )
    });
}

#[gpui::test]
async fn test_repository_subfolder_git_status(cx: &mut TestAppContext) {
    init_test(cx);
    cx.executor().allow_parking();

    let root = TempTree::new(json!({
        "my-repo": {
            // .git folder will go here
            "a.txt": "a",
            "sub-folder-1": {
                "sub-folder-2": {
                    "c.txt": "cc",
                    "d": {
                        "e.txt": "eee"
                    }
                },
            }
        },

    }));

    const C_TXT: &str = "sub-folder-1/sub-folder-2/c.txt";
    const E_TXT: &str = "sub-folder-1/sub-folder-2/d/e.txt";

    // Set up git repository before creating the worktree.
    let git_repo_work_dir = root.path().join("my-repo");
    let repo = git_init(git_repo_work_dir.as_path());
    git_add(C_TXT, &repo);
    git_commit("Initial commit", &repo);

    // Open the worktree in subfolder
    let project_root = Path::new("my-repo/sub-folder-1/sub-folder-2");
    let tree = Worktree::local(
        root.path().join(project_root),
        true,
        Arc::new(RealFs::new(None, cx.executor())),
        Default::default(),
        &mut cx.to_async(),
    )
    .await
    .unwrap();

    tree.flush_fs_events(cx).await;
    tree.flush_fs_events_in_root_git_repository(cx).await;
    cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
        .await;
    cx.executor().run_until_parked();

    // Ensure that the git status is loaded correctly
    tree.read_with(cx, |tree, _cx| {
        let snapshot = tree.snapshot();
        assert_eq!(snapshot.repositories.iter().count(), 1);
        let repo = snapshot.repositories.iter().next().unwrap();
        assert_eq!(
            repo.work_directory_abs_path.canonicalize().unwrap(),
            root.path().join("my-repo").canonicalize().unwrap()
        );

        assert_eq!(repo.status_for_path(&C_TXT.into()), None);
        assert_eq!(
            repo.status_for_path(&E_TXT.into()).unwrap().status,
            FileStatus::Untracked
        );
    });

    // Now we simulate FS events, but ONLY in the .git folder that's outside
    // of out project root.
    // Meaning: we don't produce any FS events for files inside the project.
    git_add(E_TXT, &repo);
    git_commit("Second commit", &repo);
    tree.flush_fs_events_in_root_git_repository(cx).await;
    cx.executor().run_until_parked();

    tree.read_with(cx, |tree, _cx| {
        let snapshot = tree.snapshot();
        let repos = snapshot.repositories().iter().cloned().collect::<Vec<_>>();
        assert_eq!(repos.len(), 1);
        let repo_entry = repos.into_iter().next().unwrap();

        assert!(snapshot.repositories.iter().next().is_some());

        assert_eq!(repo_entry.status_for_path(&C_TXT.into()), None);
        assert_eq!(repo_entry.status_for_path(&E_TXT.into()), None);
    });
}

#[gpui::test]
async fn test_conflicted_cherry_pick(cx: &mut TestAppContext) {
    init_test(cx);
    cx.executor().allow_parking();

    let root = TempTree::new(json!({
        "project": {
            "a.txt": "a",
        },
    }));
    let root_path = root.path();

    let tree = Worktree::local(
        root_path,
        true,
        Arc::new(RealFs::new(None, cx.executor())),
        Default::default(),
        &mut cx.to_async(),
    )
    .await
    .unwrap();

    let repo = git_init(&root_path.join("project"));
    git_add("a.txt", &repo);
    git_commit("init", &repo);

    cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
        .await;

    tree.flush_fs_events(cx).await;

    git_branch("other-branch", &repo);
    git_checkout("refs/heads/other-branch", &repo);
    std::fs::write(root_path.join("project/a.txt"), "A").unwrap();
    git_add("a.txt", &repo);
    git_commit("capitalize", &repo);
    let commit = repo
        .head()
        .expect("Failed to get HEAD")
        .peel_to_commit()
        .expect("HEAD is not a commit");
    git_checkout("refs/heads/main", &repo);
    std::fs::write(root_path.join("project/a.txt"), "b").unwrap();
    git_add("a.txt", &repo);
    git_commit("improve letter", &repo);
    git_cherry_pick(&commit, &repo);
    std::fs::read_to_string(root_path.join("project/.git/CHERRY_PICK_HEAD"))
        .expect("No CHERRY_PICK_HEAD");
    pretty_assertions::assert_eq!(
        git_status(&repo),
        collections::HashMap::from_iter([("a.txt".to_owned(), git2::Status::CONFLICTED)])
    );
    tree.flush_fs_events(cx).await;
    let conflicts = tree.update(cx, |tree, _| {
        let entry = tree.repositories.first().expect("No git entry").clone();
        entry
            .current_merge_conflicts
            .iter()
            .cloned()
            .collect::<Vec<_>>()
    });
    pretty_assertions::assert_eq!(conflicts, [RepoPath::from("a.txt")]);

    git_add("a.txt", &repo);
    // Attempt to manually simulate what `git cherry-pick --continue` would do.
    git_commit("whatevs", &repo);
    std::fs::remove_file(root.path().join("project/.git/CHERRY_PICK_HEAD"))
        .expect("Failed to remove CHERRY_PICK_HEAD");
    pretty_assertions::assert_eq!(git_status(&repo), collections::HashMap::default());
    tree.flush_fs_events(cx).await;
    let conflicts = tree.update(cx, |tree, _| {
        let entry = tree.repositories.first().expect("No git entry").clone();
        entry
            .current_merge_conflicts
            .iter()
            .cloned()
            .collect::<Vec<_>>()
    });
    pretty_assertions::assert_eq!(conflicts, []);
}

#[gpui::test]
async fn test_private_single_file_worktree(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.background_executor.clone());
    fs.insert_tree("/", json!({".env": "PRIVATE=secret\n"}))
        .await;
    let tree = Worktree::local(
        Path::new("/.env"),
        true,
        fs.clone(),
        Default::default(),
        &mut cx.to_async(),
    )
    .await
    .unwrap();
    cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
        .await;
    tree.read_with(cx, |tree, _| {
        let entry = tree.entry_for_path("").unwrap();
        assert!(entry.is_private);
    });
}

#[gpui::test]
fn test_unrelativize() {
    let work_directory = WorkDirectory::in_project("");
    pretty_assertions::assert_eq!(
        work_directory.try_unrelativize(&"crates/gpui/gpui.rs".into()),
        Some(Path::new("crates/gpui/gpui.rs").into())
    );

    let work_directory = WorkDirectory::in_project("vendor/some-submodule");
    pretty_assertions::assert_eq!(
        work_directory.try_unrelativize(&"src/thing.c".into()),
        Some(Path::new("vendor/some-submodule/src/thing.c").into())
    );

    let work_directory = WorkDirectory::AboveProject {
        absolute_path: Path::new("/projects/zed").into(),
        location_in_repo: Path::new("crates/gpui").into(),
    };

    pretty_assertions::assert_eq!(
        work_directory.try_unrelativize(&"crates/util/util.rs".into()),
        None,
    );

    pretty_assertions::assert_eq!(
        work_directory.unrelativize(&"crates/util/util.rs".into()),
        Path::new("../util/util.rs").into()
    );

    pretty_assertions::assert_eq!(work_directory.try_unrelativize(&"README.md".into()), None,);

    pretty_assertions::assert_eq!(
        work_directory.unrelativize(&"README.md".into()),
        Path::new("../../README.md").into()
    );
}

#[track_caller]
fn git_init(path: &Path) -> git2::Repository {
    let mut init_opts = RepositoryInitOptions::new();
    init_opts.initial_head("main");
    git2::Repository::init_opts(path, &init_opts).expect("Failed to initialize git repository")
}

#[track_caller]
fn git_add<P: AsRef<Path>>(path: P, repo: &git2::Repository) {
    let path = path.as_ref();
    let mut index = repo.index().expect("Failed to get index");
    index.add_path(path).expect("Failed to add file");
    index.write().expect("Failed to write index");
}

#[track_caller]
fn git_remove_index(path: &Path, repo: &git2::Repository) {
    let mut index = repo.index().expect("Failed to get index");
    index.remove_path(path).expect("Failed to add file");
    index.write().expect("Failed to write index");
}

#[track_caller]
fn git_commit(msg: &'static str, repo: &git2::Repository) {
    use git2::Signature;

    let signature = Signature::now("test", "test@zed.dev").unwrap();
    let oid = repo.index().unwrap().write_tree().unwrap();
    let tree = repo.find_tree(oid).unwrap();
    if let Ok(head) = repo.head() {
        let parent_obj = head.peel(git2::ObjectType::Commit).unwrap();

        let parent_commit = parent_obj.as_commit().unwrap();

        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            msg,
            &tree,
            &[parent_commit],
        )
        .expect("Failed to commit with parent");
    } else {
        repo.commit(Some("HEAD"), &signature, &signature, msg, &tree, &[])
            .expect("Failed to commit");
    }
}

#[track_caller]
fn git_cherry_pick(commit: &git2::Commit<'_>, repo: &git2::Repository) {
    repo.cherrypick(commit, None).expect("Failed to cherrypick");
}

#[track_caller]
fn git_stash(repo: &mut git2::Repository) {
    use git2::Signature;

    let signature = Signature::now("test", "test@zed.dev").unwrap();
    repo.stash_save(&signature, "N/A", None)
        .expect("Failed to stash");
}

#[track_caller]
fn git_reset(offset: usize, repo: &git2::Repository) {
    let head = repo.head().expect("Couldn't get repo head");
    let object = head.peel(git2::ObjectType::Commit).unwrap();
    let commit = object.as_commit().unwrap();
    let new_head = commit
        .parents()
        .inspect(|parnet| {
            parnet.message();
        })
        .nth(offset)
        .expect("Not enough history");
    repo.reset(new_head.as_object(), git2::ResetType::Soft, None)
        .expect("Could not reset");
}

#[track_caller]
fn git_branch(name: &str, repo: &git2::Repository) {
    let head = repo
        .head()
        .expect("Couldn't get repo head")
        .peel_to_commit()
        .expect("HEAD is not a commit");
    repo.branch(name, &head, false).expect("Failed to commit");
}

#[track_caller]
fn git_checkout(name: &str, repo: &git2::Repository) {
    repo.set_head(name).expect("Failed to set head");
    repo.checkout_head(None).expect("Failed to check out head");
}

#[track_caller]
fn git_status(repo: &git2::Repository) -> collections::HashMap<String, git2::Status> {
    repo.statuses(None)
        .unwrap()
        .iter()
        .map(|status| (status.path().unwrap().to_string(), status.status()))
        .collect()
}

#[track_caller]
fn check_worktree_entries(
    tree: &Worktree,
    expected_excluded_paths: &[&str],
    expected_ignored_paths: &[&str],
    expected_tracked_paths: &[&str],
    expected_included_paths: &[&str],
) {
    for path in expected_excluded_paths {
        let entry = tree.entry_for_path(path);
        assert!(
            entry.is_none(),
            "expected path '{path}' to be excluded, but got entry: {entry:?}",
        );
    }
    for path in expected_ignored_paths {
        let entry = tree
            .entry_for_path(path)
            .unwrap_or_else(|| panic!("Missing entry for expected ignored path '{path}'"));
        assert!(
            entry.is_ignored,
            "expected path '{path}' to be ignored, but got entry: {entry:?}",
        );
    }
    for path in expected_tracked_paths {
        let entry = tree
            .entry_for_path(path)
            .unwrap_or_else(|| panic!("Missing entry for expected tracked path '{path}'"));
        assert!(
            !entry.is_ignored || entry.is_always_included,
            "expected path '{path}' to be tracked, but got entry: {entry:?}",
        );
    }
    for path in expected_included_paths {
        let entry = tree
            .entry_for_path(path)
            .unwrap_or_else(|| panic!("Missing entry for expected included path '{path}'"));
        assert!(
            entry.is_always_included,
            "expected path '{path}' to always be included, but got entry: {entry:?}",
        );
    }
}

fn init_test(cx: &mut gpui::TestAppContext) {
    if std::env::var("RUST_LOG").is_ok() {
        env_logger::try_init().ok();
    }

    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        WorktreeSettings::register(cx);
    });
}

#[track_caller]
fn assert_entry_git_state(
    tree: &Worktree,
    path: &str,
    index_status: Option<StatusCode>,
    is_ignored: bool,
) {
    let entry = tree.entry_for_path(path).expect("entry {path} not found");
    let repos = tree.repositories().iter().cloned().collect::<Vec<_>>();
    assert_eq!(repos.len(), 1);
    let repo_entry = repos.into_iter().next().unwrap();
    let status = repo_entry
        .status_for_path(&path.into())
        .map(|entry| entry.status);
    let expected = index_status.map(|index_status| {
        TrackedStatus {
            index_status,
            worktree_status: StatusCode::Unmodified,
        }
        .into()
    });
    assert_eq!(
        status, expected,
        "expected {path} to have git status: {expected:?}"
    );
    assert_eq!(
        entry.is_ignored, is_ignored,
        "expected {path} to have is_ignored: {is_ignored}"
    );
}
