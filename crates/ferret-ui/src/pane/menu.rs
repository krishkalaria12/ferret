//! The pane's context menus (§8): the empty-space menu, the icon-tile
//! menu, and the entry items both share with the list's row menu.

use super::*;

pub(super) struct SpaceMenu {
    position: gpui::Point<gpui::Pixels>,
    menu: gpui::Entity<PopupMenu>,
    _dismiss: Subscription,
}

impl FilePane {
    /// Right-click on empty space opens the pane menu: create, paste, sort,
    /// toggles. Rows and tiles claim the event first, in the bubble phase, so
    /// this only has to notice that they did.
    pub(super) fn open_space_menu(
        &mut self,
        event: &gpui::MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // List view routes row hits through the table's own menu.
        if self.view_mode == ViewMode::List && self.table.read(cx).right_clicked_row().is_some() {
            return;
        }
        // Icon tiles and column rows open their menu on the way out to here;
        // a menu already placed at this event's position is theirs.
        let origin = *self.pane_origin.lock();
        if self
            .space_menu
            .as_ref()
            .is_some_and(|menu| menu.position == event.position - origin)
        {
            return;
        }
        self.open_menu_at(None, event, window, cx);
    }

    /// The pane menu, optionally aimed at one entry (icon-view tiles).
    pub(super) fn open_menu_at(
        &mut self,
        target: Option<(VfsPath, Option<ferret_vfs::SymlinkTarget>)>,
        event: &gpui::MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focus = gpui::Focusable::focus_handle(self.table.read(cx), cx);
        let servers = send_to_targets(cx);
        let (key, order, show_hidden, selected, dir) = self.folder.read_with(cx, |f, _| {
            (
                f.sort_key(),
                f.sort_order(),
                f.show_hidden(),
                f.selection_count(),
                f.dir.clone(),
            )
        });
        let trashable = dir.is_local();
        let symlinkable = cx
            .global::<crate::state::FerretState>()
            .fs
            .capabilities_for(&dir)
            .create_symlinks;

        let menu = PopupMenu::build(window, cx, move |menu, window, cx| {
            use gpui_component::menu::PopupMenuItem;
            let menu = menu.action_context(focus);
            let menu = if let Some((target, target_kind)) = target.clone() {
                entry_menu_items(
                    menu,
                    selected,
                    trashable,
                    symlinkable,
                    Some((target, target_kind)),
                    servers.clone(),
                )
                .separator()
            } else {
                menu
            };
            let menu = menu
                .item(PopupMenuItem::new("New Folder").action(Box::new(actions::NewFolder)))
                .item(PopupMenuItem::new("New File").action(Box::new(actions::NewFile)));
            let menu = if symlinkable {
                menu.item(PopupMenuItem::new("New Symlink…").action(Box::new(
                    actions::NewSymlink {
                        target: None,
                        target_kind: None,
                    },
                )))
            } else {
                menu
            };
            menu.separator()
                .item(
                    PopupMenuItem::new("Open in Terminal")
                        .action(Box::new(actions::OpenInTerminal)),
                )
                .separator()
                .item(PopupMenuItem::new("Paste").action(Box::new(actions::Paste)))
                .separator()
                .submenu("Sort By", window, cx, move |sort_menu, _, _| {
                    sort_menu
                        .menu_with_check(
                            "Name",
                            key == SortKey::Name,
                            std::boxed::Box::new(actions::SortByName),
                        )
                        .menu_with_check(
                            "Date Modified",
                            key == SortKey::Modified,
                            std::boxed::Box::new(actions::SortByModified),
                        )
                        .menu_with_check(
                            "Size",
                            key == SortKey::Size,
                            std::boxed::Box::new(actions::SortBySize),
                        )
                        .separator()
                        .menu_with_check(
                            "Ascending",
                            order == SortOrder::Ascending,
                            std::boxed::Box::new(actions::SortAscending),
                        )
                        .menu_with_check(
                            "Descending",
                            order == SortOrder::Descending,
                            std::boxed::Box::new(actions::SortDescending),
                        )
                })
                .separator()
                .menu_with_check(
                    "Show Hidden Files",
                    show_hidden,
                    std::boxed::Box::new(actions::ToggleHidden),
                )
                .item(PopupMenuItem::new("Refresh").action(std::boxed::Box::new(actions::Refresh)))
        });

        let dismiss = cx.subscribe(
            &menu,
            |this: &mut Self,
             _: gpui::Entity<PopupMenu>,
             _: &DismissEvent,
             cx: &mut Context<Self>| {
                if this.space_menu.take().is_some() {
                    cx.notify();
                }
            },
        );

        let origin = *self.pane_origin.lock();
        self.space_menu = Some(SpaceMenu {
            position: event.position - origin,
            menu,
            _dismiss: dismiss,
        });
        cx.notify();
    }

    pub(super) fn space_menu_overlay(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(space_menu) = &self.space_menu else {
            return div().into_any_element();
        };
        // A full-pane click catcher closes the menu on any click; the menu
        // itself sits above it at the cursor.
        div()
            .absolute()
            .inset_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseDownEvent, _, cx| {
                    if this.space_menu.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _: &gpui::MouseDownEvent, _, cx| {
                    if this.space_menu.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .child(
                div()
                    .absolute()
                    .left(space_menu.position.x)
                    .top(space_menu.position.y)
                    .child(space_menu.menu.clone()),
            )
            .into_any_element()
    }
}

/// The row and selection halves of §8's three menus.
///
///
/// `trashable` is false on a mount: a server has no trash, so offering to move
/// something there would be offering an action that cannot happen. The
/// permanent entry is what remains, and it confirms every time.
///
/// `servers` are the saved connections a local selection can be sent to, as
/// `(label, id)` pairs. Passed in rather than read from the global here so
/// this stays a pure builder that a test can call.
pub(super) fn entry_menu_items(
    menu: PopupMenu,
    selected: usize,
    trashable: bool,
    symlinkable: bool,
    target: Option<(VfsPath, Option<ferret_vfs::SymlinkTarget>)>,
    servers: Vec<(String, String)>,
) -> PopupMenu {
    let multiple = selected > 1;
    let count = selected.min(999);

    let menu = menu
        .item(
            PopupMenuItem::new(if multiple {
                format!("Open {count} Items")
            } else {
                "Open".to_string()
            })
            .action(Box::new(actions::Open)),
        )
        .separator()
        .item(PopupMenuItem::new("Cut").action(Box::new(actions::Cut)))
        .item(PopupMenuItem::new("Copy").action(Box::new(actions::Copy)))
        .separator();

    let menu = menu.item(
        PopupMenuItem::new(if multiple {
            "Batch Rename…"
        } else {
            "Rename…"
        })
        .action(Box::new(actions::Rename)),
    );

    let menu = if symlinkable
        && selected == 1
        && let Some((target, target_kind)) = target
    {
        menu.item(
            PopupMenuItem::new("New Symlink to Selected…").action(Box::new(actions::NewSymlink {
                target: Some(target),
                target_kind,
            })),
        )
    } else {
        menu
    };

    // "Send to" (F-22), offered only for local files: a server-to-server copy
    // would have to relay through this machine, which is a different
    // performance story and belongs with dual pane (F-05).
    //
    // Flat items rather than a submenu, because `PopupMenu::submenu` needs a
    // `Context<PopupMenu>` and the list view builds its row menu from the
    // table's own context. One level that works in both places beats two
    // levels in one of them.
    let menu = if trashable && !servers.is_empty() {
        let mut menu = menu.separator();
        for (label, id) in servers {
            menu = menu.item(
                PopupMenuItem::new(format!("Send to {label}"))
                    .action(Box::new(actions::SendToConnection { id })),
            );
        }
        menu.separator()
    } else {
        menu
    };

    let menu = if trashable {
        menu.item(
            PopupMenuItem::new(if multiple {
                format!("Move {count} Items to Trash")
            } else {
                "Move to Trash".to_string()
            })
            .action(Box::new(actions::Trash)),
        )
    } else {
        menu
    };
    menu.item(
        PopupMenuItem::new("Delete Permanently…").action(Box::new(actions::DeletePermanently)),
    )
}

/// The saved connections a "Send to" item can name.
///
/// Read from the global here because the menu is built inside a closure that
/// has an `App` and no view to ask.
pub(super) fn send_to_targets(cx: &App) -> Vec<(String, String)> {
    cx.global::<crate::state::FerretState>()
        .connections
        .connections()
        .iter()
        .map(|connection| (connection.display_name(), connection.id.0.clone()))
        .collect()
}
