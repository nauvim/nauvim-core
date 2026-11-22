use nauvim_core::{Client, Engine, EngineEvent, Frame, GridId, State};

use super::is_clipboard_detection_error;

use nvim_rpc_plugin::api;
use nvim_rpc_ui::{Event, Grid, GridPlacementKind};

use tokio::time::{Duration, timeout};

const NORMAL: &str = "checkpoint normal";
const FLOAT: &str = "checkpoint float";

#[derive(Debug, PartialEq, Eq)]
pub struct RetainedGrids {
    normal: GridId,
    floating: GridId,
}

pub async fn leave_inactive_tab(client: &Client) {
    let executed = timeout(
        Duration::from_secs(5),
        client.nvim().request::<api::NvimExecLua, bool>((
            r#"
            vim.api.nvim_buf_set_lines(0, 0, -1, false, {'checkpoint normal'})
            local buf = vim.api.nvim_create_buf(false, true)
            vim.api.nvim_buf_set_lines(buf, 0, -1, false, {'checkpoint float'})
            vim.api.nvim_open_win(buf, true, {
                relative = 'editor', width = 23, height = 5, row = 3, col = 5,
                style = 'minimal', border = 'none',
            })
            vim.api.nvim__redraw({ flush = true })
            vim.cmd('tabnew')
            vim.api.nvim__redraw({ flush = true })

            local channel = vim.api.nvim_list_uis()[1].chan
            vim.rpcnotify(channel, 'redraw', {'bell', {}}, {'flush', {}})
            -- Core answers this unaddressed request after preceding redraws.
            vim.rpcrequest(channel, 'test.redraw_barrier', vim.NIL)
            return true
            "#,
            (),
        )),
    )
    .await
    .expect("setting up the inactive tab timed out")
    .unwrap();
    assert!(executed);
}

pub async fn next_frame(engine: &mut Engine) -> Frame {
    timeout(Duration::from_secs(5), async {
        loop {
            match engine.next_event().await {
                EngineEvent::Frame(frame) => return frame,
                EngineEvent::Error(e) if is_clipboard_detection_error(&e) => {}
                EngineEvent::Error(e) | EngineEvent::Welcome { error: Some(e) } => {
                    panic!("unexpected engine error: {e}")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("Neovim did not publish a frame")
}

pub async fn restored_grids(engine: &mut Engine) -> RetainedGrids {
    let frame = next_frame(engine).await;
    assert!(frame.damage.full, "the restored UI needs a full repaint");
    assert!(
        frame
            .events
            .iter()
            .all(|event| !matches!(event, Event::Bell)),
        "the checkpoint replayed a bell from the previous attachment"
    );
    hidden_grids(&frame.state).expect("first frame is missing the hidden grids")
}

pub fn hidden_grids(state: &State) -> Option<RetainedGrids> {
    let grids = markers(state)?;
    [grids.normal, grids.floating]
        .into_iter()
        .all(|id| state.placements.get(&id).is_none_or(|p| p.hidden))
        .then_some(grids)
}

pub async fn revisit_inactive_tab(engine: &mut Engine, client: &Client, grids: &RetainedGrids) {
    client.command("tabnext").await.unwrap();
    timeout(Duration::from_secs(5), async {
        loop {
            let frame = next_frame(engine).await;
            let normal = frame.state.placements.get(&grids.normal);
            let floating = frame.state.placements.get(&grids.floating);
            if normal
                .is_some_and(|p| !p.hidden && matches!(p.kind, GridPlacementKind::Normal { .. }))
                && floating.is_some_and(|p| {
                    !p.hidden && matches!(p.kind, GridPlacementKind::Floating { .. })
                })
            {
                assert_eq!(markers(&frame.state).as_ref(), Some(grids));
                break;
            }
        }
    })
    .await
    .expect("the restored normal and floating grids did not become visible again");
}

pub fn assert_no_markers(frame: &Frame) {
    for marker in [NORMAL, FLOAT] {
        assert!(
            find_marker(&frame.state, marker).is_none(),
            "unexpected {marker:?}"
        );
    }
}

fn markers(state: &State) -> Option<RetainedGrids> {
    Some(RetainedGrids {
        normal: find_marker(state, NORMAL)?,
        floating: find_marker(state, FLOAT)?,
    })
}

fn find_marker(state: &State, marker: &str) -> Option<GridId> {
    fn contains(grid: &Grid, marker: &str) -> bool {
        grid.rows.iter().any(|row| {
            row.cells.windows(marker.len()).any(|cells| {
                cells
                    .iter()
                    .zip(marker.bytes())
                    .all(|(cell, byte)| cell.text.as_bytes() == [byte])
            })
        })
    }

    state
        .grids
        .iter()
        .find_map(|(&id, grid)| contains(grid, marker).then_some(id))
}
