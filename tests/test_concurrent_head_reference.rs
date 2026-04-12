use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::thread;
use tempfile::TempDir;

fn init_git_repo(path: &Path) {
    Command::new("git")
        .args(["init"])
        .arg(path)
        .output()
        .unwrap();
    Command::new("git")
        .args(["config", "user.email", "test@example.com"])
        .current_dir(path)
        .output()
        .unwrap();
    Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(path)
        .output()
        .unwrap();
    // Create initial commit to establish HEAD
    std::fs::write(path.join("file.txt"), "hello").unwrap();
    Command::new("git")
        .args(["add", "."])
        .current_dir(path)
        .output()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(path)
        .output()
        .unwrap();
}

// Create a worktree using git commands directly
fn create_test_worktree(
    project_root: &Path,
    agent_id: &str,
    task_id: &str,
) -> Result<std::path::PathBuf, String> {
    let worktree_dir = project_root.join(".wg-worktrees").join(agent_id);
    let branch = format!("wg/{}/{}", agent_id, task_id);

    // Clean up any existing worktree/branch first
    let _ = Command::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(&worktree_dir)
        .current_dir(project_root)
        .output();
    let _ = Command::new("git")
        .args(["branch", "-D", &branch])
        .current_dir(project_root)
        .output();

    // Ensure parent directory exists
    if let Some(parent) = worktree_dir.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create parent dir: {}", e))?;
    }

    // Create worktree from HEAD
    let output = Command::new("git")
        .args(["worktree", "add"])
        .arg(&worktree_dir)
        .args(["-b", &branch, "HEAD"])
        .current_dir(project_root)
        .output()
        .map_err(|e| format!("Failed to run git worktree add: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git worktree add failed: {}", stderr.trim()));
    }

    Ok(worktree_dir)
}

// Remove worktree using git commands directly
fn remove_test_worktree(
    project_root: &Path,
    worktree_path: &Path,
    branch: &str,
) -> Result<(), String> {
    // Force-remove the worktree
    let _ = Command::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(worktree_path)
        .current_dir(project_root)
        .output();

    // Delete the branch
    let _ = Command::new("git")
        .args(["branch", "-D", branch])
        .current_dir(project_root)
        .output();

    // Prune stale worktree entries
    let _ = Command::new("git")
        .args(["worktree", "prune"])
        .current_dir(project_root)
        .output();

    Ok(())
}

#[test]
fn test_concurrent_worktree_creation_head_reference() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    init_git_repo(&project);

    let wg_dir = project.join(".workgraph");
    std::fs::create_dir_all(&wg_dir).unwrap();

    let project_arc = Arc::new(project);
    let wg_dir_arc = Arc::new(wg_dir);
    let num_agents = 5;

    let mut handles = vec![];

    // Spawn multiple threads that create worktrees simultaneously
    for i in 0..num_agents {
        let project_clone = Arc::clone(&project_arc);
        let _wg_dir_clone = Arc::clone(&wg_dir_arc);

        let handle = thread::spawn(move || {
            let agent_id = format!("agent-{}", i);
            let task_id = format!("task-{}", i);

            // Attempt to create worktree
            let result = create_test_worktree(&*project_clone, &agent_id, &task_id);

            match result {
                Ok(worktree_path) => {
                    // Verify worktree was created successfully
                    assert!(worktree_path.exists(), "Worktree path should exist");
                    assert!(
                        worktree_path.join("file.txt").exists(),
                        "Source files should be checked out"
                    );

                    // Verify we can run git commands in the worktree and HEAD reference works
                    let git_status = Command::new("git")
                        .args(["status", "--porcelain"])
                        .current_dir(&worktree_path)
                        .output()
                        .expect("git status should work in worktree");

                    assert!(
                        git_status.status.success(),
                        "git status should succeed in worktree"
                    );

                    // Verify HEAD reference is accessible
                    let git_head = Command::new("git")
                        .args(["rev-parse", "HEAD"])
                        .current_dir(&worktree_path)
                        .output()
                        .expect("git rev-parse HEAD should work");

                    assert!(git_head.status.success(), "HEAD should be accessible");
                    assert!(
                        !git_head.stdout.is_empty(),
                        "HEAD should return a commit hash"
                    );

                    // Cleanup
                    let branch = format!("wg/{}/{}", agent_id, task_id);
                    remove_test_worktree(&*project_clone, &worktree_path, &branch).unwrap();

                    Ok(())
                }
                Err(e) => Err(format!("Agent {}: Failed to create worktree: {}", i, e)),
            }
        });

        handles.push(handle);
    }

    // Wait for all threads and collect results
    let mut results = vec![];
    for handle in handles {
        results.push(handle.join().unwrap());
    }

    // Verify all agents succeeded
    for (i, result) in results.iter().enumerate() {
        match result {
            Ok(_) => println!("Agent {} succeeded", i),
            Err(e) => panic!("Agent {} failed: {}", i, e),
        }
    }
}

#[test]
fn test_head_reference_under_rapid_agent_turnover() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    init_git_repo(&project);

    let wg_dir = project.join(".workgraph");
    std::fs::create_dir_all(&wg_dir).unwrap();

    // Simulate rapid create/destroy cycles
    for cycle in 0..10 {
        let agent_id = format!("agent-cycle-{}", cycle);
        let task_id = format!("task-cycle-{}", cycle);

        // Create worktree
        let worktree_path = create_test_worktree(&project, &agent_id, &task_id)
            .expect("Worktree creation should succeed");

        // Verify HEAD is accessible
        let git_rev_parse = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&worktree_path)
            .output()
            .expect("git rev-parse HEAD should work");

        assert!(
            git_rev_parse.status.success(),
            "HEAD should be accessible in cycle {}",
            cycle
        );
        assert!(
            !git_rev_parse.stdout.is_empty(),
            "HEAD should return a commit hash in cycle {}",
            cycle
        );

        // Immediately remove worktree
        let branch = format!("wg/{}/{}", agent_id, task_id);
        remove_test_worktree(&project, &worktree_path, &branch)
            .expect("Worktree removal should succeed");

        assert!(
            !worktree_path.exists(),
            "Worktree should be cleaned up in cycle {}",
            cycle
        );
    }
}

#[test]
fn test_worktree_creation_with_git_operations_in_progress() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    init_git_repo(&project);

    let wg_dir = project.join(".workgraph");
    std::fs::create_dir_all(&wg_dir).unwrap();

    let project_arc = Arc::new(project);
    let wg_dir_arc = Arc::new(wg_dir);

    let mut handles = vec![];

    // Start a background thread doing git operations
    let project_bg = Arc::clone(&project_arc);
    let bg_handle = thread::spawn(move || {
        for i in 0..20 {
            // Simulate ongoing git activity
            let filename = format!("bg-file-{}.txt", i);
            std::fs::write(project_bg.join(&filename), format!("content {}", i)).unwrap();

            let add_result = Command::new("git")
                .args(["add", &filename])
                .current_dir(&*project_bg)
                .output();

            if let Ok(output) = add_result {
                if output.status.success() {
                    let _ = Command::new("git")
                        .args(["commit", "-m", &format!("Background commit {}", i)])
                        .current_dir(&*project_bg)
                        .output();
                }
            }

            thread::sleep(std::time::Duration::from_millis(10));
        }
    });

    // Meanwhile, try to create worktrees
    for i in 0..3 {
        let project_clone = Arc::clone(&project_arc);
        let _wg_dir_clone = Arc::clone(&wg_dir_arc);

        let handle = thread::spawn(move || {
            thread::sleep(std::time::Duration::from_millis(i * 50)); // Stagger starts

            let agent_id = format!("concurrent-agent-{}", i);
            let task_id = format!("concurrent-task-{}", i);

            let result = create_test_worktree(&*project_clone, &agent_id, &task_id);

            match result {
                Ok(worktree_path) => {
                    // Verify HEAD reference works despite concurrent git operations
                    let git_log = Command::new("git")
                        .args(["log", "--oneline", "-n", "1"])
                        .current_dir(&worktree_path)
                        .output()
                        .expect("git log should work");

                    assert!(
                        git_log.status.success(),
                        "git log should succeed for agent {}",
                        i
                    );
                    assert!(
                        !git_log.stdout.is_empty(),
                        "git log should return commits for agent {}",
                        i
                    );

                    // Cleanup
                    let branch = format!("wg/{}/{}", agent_id, task_id);
                    remove_test_worktree(&*project_clone, &worktree_path, &branch).unwrap();
                    Ok(())
                }
                Err(e) => Err(format!("Agent {}: {}", i, e)),
            }
        });

        handles.push(handle);
    }

    // Wait for worktree operations
    for handle in handles {
        handle
            .join()
            .unwrap()
            .expect("Worktree operations should succeed");
    }

    // Wait for background operations
    bg_handle.join().unwrap();
}
