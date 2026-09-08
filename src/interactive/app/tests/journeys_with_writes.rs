use crate::interactive::app::tests::utils::{
    WritableFixture, initialized_app_and_terminal_from_paths, into_codes, into_events,
    new_test_terminal,
};
use crate::interactive::terminal::TerminalApp;
use crate::interactive::widgets::Language;
use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use dua::{ByteFormat, Config, WalkOptions};
use pretty_assertions::assert_eq;
use std::{collections::BTreeSet, fs};
use tempfile::TempDir;

fn marked_file_names(app: &TerminalApp, message: &str) -> BTreeSet<String> {
    app.window
        .mark
        .as_ref()
        .expect(message)
        .marked()
        .values()
        .map(|entry| {
            entry
                .path
                .file_name()
                .expect("marked path has a final component")
                .to_string_lossy()
                .to_string()
        })
        .collect()
}

#[test]
fn deletion_during_scan_ignores_queued_descendants() -> anyhow::Result<()> {
    use crate::interactive::app::tests::utils::{
        index_by_name, untraversed_app_and_terminal_with_closure,
    };
    use crate::interactive::app::tree_view::TreeView;
    use dua::traverse::TraversalEvent;
    use std::{collections::VecDeque, io, path::Path, sync::Arc};

    for reuse_slot in [false, true] {
        let dir = tempfile::tempdir()?;
        let removed = dir.path().join("removed");
        let kept = dir.path().join("kept");
        fs::create_dir_all(removed.join("nested/deep"))?;
        fs::write(removed.join("nested/deep/file"), b"deleted")?;
        fs::write(&kept, b"keep")?;
        let (_, mut app) = untraversed_app_and_terminal_with_closure(
            &[removed.clone(), kept.clone()],
            Path::to_path_buf,
        )?;
        app.traverse()?;

        // Queue the complete walk so deletion always precedes descendant integration.
        let scan = &mut app.state.scan.as_mut().unwrap().active_traversal;
        let (mut removed_events, mut kept_events): (VecDeque<_>, VecDeque<_>) = scan
            .event_rx
            .iter()
            .filter(|event| !matches!(event, TraversalEvent::Finished))
            .partition(|event| matches!(event, TraversalEvent::Entry(_, _, _, 0)));
        scan.integrate_traversal_event(&mut app.traversal, removed_events.pop_front().unwrap());
        let removed_index = index_by_name(&app, &removed);
        let mut tree_view = TreeView {
            traversal: &mut app.traversal,
            glob_tree_root: None,
            glob_matches: None,
        };
        fs::remove_dir_all(&removed)?;
        app.state
            .delete_entries_in_traversal(removed_index, &mut tree_view);
        assert!(!removed.exists());

        let scan = &mut app.state.scan.as_mut().unwrap().active_traversal;
        if reuse_slot {
            for event in kept_events.drain(..) {
                scan.integrate_traversal_event(&mut app.traversal, event);
            }
            assert_eq!(
                app.traversal.tree.name(removed_index).as_deref(),
                Some(kept.as_path()),
                "the deleted parent slot is now an unrelated file"
            );
        }
        for event in removed_events.into_iter().chain(kept_events) {
            scan.integrate_traversal_event(&mut app.traversal, event);
        }
        scan.integrate_traversal_event(
            &mut app.traversal,
            TraversalEvent::Entry(
                Err(io::Error::other("directory removed during the scan")),
                Arc::new(removed),
                0,
                0,
            ),
        );
        assert_eq!(
            scan.integrate_traversal_event(&mut app.traversal, TraversalEvent::Finished),
            Some(true)
        );
        assert_eq!(scan.stats.total_bytes, Some(4));
        assert_eq!(scan.stats.io_errors, 1);
        let roots = scan.root_nodes().unwrap();
        assert_eq!(roots.len(), 1, "deleted roots are excluded from snapshots");
        assert_eq!(
            app.traversal.tree.name(roots[0]).as_deref(),
            Some(kept.as_path())
        );
        assert!(!app.traversal.tree.data(roots[0]).unwrap().metadata_io_error);
        assert_eq!(app.traversal.tree.len(), 2);
        assert_eq!(
            app.traversal
                .tree
                .data(app.traversal.root_index)
                .unwrap()
                .entry_count,
            Some(1)
        );
    }
    Ok(())
}

#[test]
fn deleting_the_directory_being_refreshed_cancels_its_scan() -> Result<()> {
    use crate::interactive::app::{
        state::FilesystemScan, tests::utils::index_by_name, tree_view::TreeView,
    };
    use dua::traverse::BackgroundTraversal;

    let dir = tempfile::tempdir()?;
    let removed = dir.path().join("removed");
    fs::create_dir(&removed)?;
    fs::write(removed.join("file"), b"content")?;
    let (_, mut app) = initialized_app_and_terminal_from_paths(std::slice::from_ref(&removed))?;
    let removed_index = index_by_name(&app, &removed);
    app.state.scan = Some(FilesystemScan {
        active_traversal: BackgroundTraversal::start(
            removed_index,
            &app.state.walk_options,
            vec![removed.clone()],
            None,
            true,
            false,
        )?,
        previous_selection: None,
        snapshot_export: None,
    });
    fs::remove_dir_all(removed)?;
    app.state.delete_entries_in_traversal(
        removed_index,
        &mut TreeView {
            traversal: &mut app.traversal,
            glob_tree_root: None,
            glob_matches: None,
        },
    );
    assert!(app.state.scan.is_none());
    assert_eq!(app.traversal.tree.len(), 1);
    assert_eq!(app.state.navigation().view_root, app.traversal.root_index);
    Ok(())
}

#[test]
#[cfg(not(target_os = "windows"))] // it stopped working here, don't know if it's truly broken or if it's the test. Let's wait for windows users to report.
fn basic_user_journey_with_deletion() -> Result<()> {
    use crate::interactive::app::tests::utils::into_events;

    let fixture = WritableFixture::from("sample-02");
    let (mut terminal, mut app) =
        initialized_app_and_terminal_from_paths(std::slice::from_ref(&fixture.root))?;

    // With a selection of items
    app.process_events(&mut terminal, into_codes("doddd"))?;

    assert_eq!(
        app.window.mark.as_ref().map(|p| p.marked().len()),
        Some(4),
        "expecting 4 selected items, the parent dir, and some children"
    );

    assert!(fixture.as_ref().is_dir(), "expecting fixture root to exist");

    // When selecting the marker window and pressing the combination to delete entries
    app.process_events(
        &mut terminal,
        into_events([
            Event::Key(KeyCode::Tab.into()),
            Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
        ]),
    )?;
    assert!(
        app.window.mark.is_none(),
        "the marker pane is gone as all items have been removed"
    );
    assert_eq!(
        app.state.navigation().selected,
        None,
        "nothing is left to be selected"
    );
    assert_eq!(
        app.state.navigation().view_root,
        app.traversal.root_index,
        "the only root left is the top-level"
    );
    assert!(
        !fixture.as_ref().is_dir(),
        "the directory should have been deleted",
    );
    Ok(())
}

#[test]
#[cfg(unix)]
fn gitignored_entries_are_marked_with_dedicated_key() -> Result<()> {
    let fixture = TempDir::new()?;
    let root = fixture.path();
    fs::create_dir_all(root.join(".git/objects"))?;
    fs::create_dir_all(root.join(".git/refs/heads"))?;
    fs::write(root.join(".git/HEAD"), b"ref: refs/heads/main\n")?;
    fs::write(
        root.join(".git/config"),
        b"[core]
	repositoryformatversion = 0
	filemode = true
	bare = false
",
    )?;
    fs::write(
        root.join(".gitignore"),
        b"ignored.log
ignored_dir/
ignored-link
target/
remove.tmp
$precious.tmp
!keep.tmp
",
    )?;
    fs::write(root.join("ignored.log"), [])?;
    fs::create_dir_all(root.join("ignored_dir"))?;
    fs::write(root.join("ignored_dir/file"), [])?;
    fs::write(root.join("remove.tmp"), [])?;
    fs::write(root.join("precious.tmp"), [])?;
    fs::write(root.join("keep.tmp"), [])?;
    std::os::unix::fs::symlink(root.join("keep.tmp"), root.join("ignored-link"))?;
    fs::create_dir_all(root.join("target/debug"))?;
    fs::write(root.join("target/debug/app"), [])?;
    fs::write(root.join("target/output.bin"), [])?;

    let mut terminal = new_test_terminal()?;
    let walk_options = WalkOptions {
        threads: 1,
        apparent_size: true,
        count_hard_links: false,
        cross_filesystems: false,
        ignore_dirs: BTreeSet::default(),
        ignore_patterns: None,
        metadata_options: dua::TraversalOptions::default(),
    };
    let (_key_send, key_receive) = crossbeam::channel::bounded(0);
    let mut app = TerminalApp::initialize(
        &mut terminal,
        walk_options,
        ByteFormat::Metric,
        true,
        vec![root.to_owned()],
        None,
        Config::default(),
        dua::traverse::Traversal::new(),
        None,
    )?;
    app.state.language = Language::English;
    app.traverse()?;
    app.run_until_traversed(&mut terminal, key_receive)?;

    app.process_events(&mut terminal, into_codes("o"))?;

    let gitignored_names = app
        .state
        .entries
        .iter()
        .filter(|entry| {
            app.state
                .gitignored_entries
                .as_ref()
                .is_some_and(|entries| entries.contains(&entry.index))
        })
        .map(|entry| entry.name.to_string_lossy().to_string())
        .collect::<BTreeSet<_>>();

    assert_eq!(
        gitignored_names,
        BTreeSet::from([
            "ignored.log".to_string(),
            "ignored_dir".to_string(),
            "ignored-link".to_string(),
            // "precious.tmp".to_string(), # precious file is notably absent from highlighted files
            "remove.tmp".to_string(),
            "target".to_string(),
        ])
    );
    assert_eq!(
        app.state
            .cleanup_candidates
            .as_ref()
            .map_or(0, BTreeSet::len),
        1,
        "built-in cleanup candidates stay separate"
    );
    assert_eq!(
        app.state.message.as_deref(),
        Some("1 cleanup, 5 gitignored"),
        "footer message describes both annotation types"
    );
    app.process_events(&mut terminal, into_codes("i"))?;
    assert!(
        app.state.gitignored_entries.is_none(),
        "gitignored entry detection can be disabled"
    );
    assert_eq!(
        app.state.message.as_deref(),
        Some("1 cleanup candidate"),
        "footer message drops gitignore details when disabled"
    );
    app.process_events(&mut terminal, into_codes("i"))?;
    assert_eq!(
        app.state.message.as_deref(),
        Some("1 cleanup, 5 gitignored"),
        "gitignored entry detection can be enabled again"
    );

    let target_index = app
        .state
        .entries
        .iter()
        .find(|entry| entry.name == std::path::Path::new("target"))
        .expect("target directory is visible")
        .index;
    app.state.navigation_mut().select(Some(target_index));
    app.process_events(&mut terminal, into_codes("o"))?;

    let target_gitignored_names = app
        .state
        .entries
        .iter()
        .filter(|entry| {
            app.state
                .gitignored_entries
                .as_ref()
                .is_some_and(|entries| entries.contains(&entry.index))
        })
        .map(|entry| entry.name.to_string_lossy().to_string())
        .collect::<BTreeSet<_>>();

    assert_eq!(
        target_gitignored_names,
        BTreeSet::from(["debug".to_string(), "output.bin".to_string()]),
        "entries inside an ignored directory are ignored as well as we use repository discovery"
    );

    app.process_events(&mut terminal, into_codes("u"))?;
    app.process_events(&mut terminal, into_codes("I"))?;

    assert_eq!(
        marked_file_names(&app, "gitignored entries are marked"),
        BTreeSet::from([
            "ignored.log".to_string(),
            "ignored_dir".to_string(),
            "ignored-link".to_string(),
            "remove.tmp".to_string(),
            "target".to_string(),
        ])
    );

    Ok(())
}

#[test]
#[cfg(not(target_os = "windows"))]
fn cleanup_candidates_are_marked_with_one_key_after_entering_project_dir() -> Result<()> {
    let fixture = TempDir::new()?;
    let root = fixture.path();
    fs::create_dir_all(root.join("target/debug"))?;
    fs::write(root.join("target/debug/app"), [])?;
    fs::create_dir_all(root.join("node_modules/package"))?;
    fs::write(root.join("node_modules/package/index.js"), [])?;
    fs::create_dir_all(root.join("__pycache__"))?;
    fs::write(root.join("__pycache__/module.pyc"), [])?;
    fs::create_dir_all(root.join("build"))?;
    fs::write(root.join("build/release-artifact"), [])?;

    let mut terminal = new_test_terminal()?;
    let walk_options = WalkOptions {
        threads: 1,
        apparent_size: true,
        count_hard_links: false,
        cross_filesystems: false,
        ignore_dirs: BTreeSet::default(),
        ignore_patterns: None,
        metadata_options: dua::TraversalOptions::default(),
    };
    let (_key_send, key_receive) = crossbeam::channel::bounded(0);
    let mut app = TerminalApp::initialize(
        &mut terminal,
        walk_options,
        ByteFormat::Metric,
        true,
        vec![root.to_owned()],
        None,
        Config::default(),
        dua::traverse::Traversal::new(),
        None,
    )?;
    app.state.language = Language::English;
    app.traverse()?;
    app.run_until_traversed(&mut terminal, key_receive)?;

    app.process_events(&mut terminal, into_codes("o"))?;

    assert_eq!(
        app.state
            .cleanup_candidates
            .as_ref()
            .map_or(0, BTreeSet::len),
        3
    );
    app.process_events(&mut terminal, into_codes("t"))?;
    assert!(
        app.state.cleanup_candidates.is_none(),
        "cleanup candidate detection can be disabled"
    );
    app.process_events(&mut terminal, into_codes("t"))?;
    assert_eq!(
        app.state
            .cleanup_candidates
            .as_ref()
            .map_or(0, BTreeSet::len),
        3,
        "cleanup candidate detection can be enabled again"
    );

    app.process_events(
        &mut terminal,
        into_events([
            Event::Key(KeyCode::Char('/').into()),
            Event::Key(KeyCode::Char('t').into()),
            Event::Key(KeyCode::Char('a').into()),
            Event::Key(KeyCode::Char('r').into()),
            Event::Key(KeyCode::Char('g').into()),
            Event::Key(KeyCode::Char('e').into()),
            Event::Key(KeyCode::Char('t').into()),
            Event::Key(KeyCode::Enter.into()),
        ]),
    )?;
    assert!(
        app.state
            .cleanup_candidates
            .as_ref()
            .is_some_and(BTreeSet::is_empty),
        "glob views should not offer cleanup candidates"
    );

    app.process_events(
        &mut terminal,
        into_events([Event::Key(KeyCode::Char('q').into())]),
    )?;
    assert_eq!(
        app.state
            .cleanup_candidates
            .as_ref()
            .map_or(0, BTreeSet::len),
        3
    );

    app.process_events(&mut terminal, into_codes("X"))?;
    app.process_events(
        &mut terminal,
        into_events([
            Event::Key(KeyCode::Tab.into()),
            Event::Key(KeyCode::Char('a').into()),
        ]),
    )?;
    app.process_events(&mut terminal, into_codes("X"))?;

    assert_eq!(
        marked_file_names(&app, "cleanup candidates are marked"),
        BTreeSet::from([
            "__pycache__".to_string(),
            "node_modules".to_string(),
            "target".to_string(),
        ])
    );
    Ok(())
}
