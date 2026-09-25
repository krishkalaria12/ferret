use super::*;

impl Browser {
    pub(in crate::browser) fn open_batch_rename(
        &mut self,
        sources: Arc<[VfsPath]>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if sources.len() < 2 || window.has_active_dialog(cx) {
            return;
        }
        let folder = self.active().folder.clone();
        if folder.read(cx).is_busy() {
            window.push_notification(
                Notification::error("Wait for the folder listing to finish before renaming."),
                cx,
            );
            return;
        }
        let fs = cx.global::<FerretState>().fs.clone();
        if sources
            .iter()
            .any(|path| !fs.capabilities_for(path).atomic_rename_noreplace)
        {
            window.push_notification(
                Notification::error(format!(
                    "Batch rename cannot safely use {}: this filesystem has no atomic no-replace rename.",
                    sources[0]
                )),
                cx,
            );
            return;
        }
        let dialog = match &self.batch_rename {
            Some(existing) => {
                existing.update(cx, |dialog, cx| {
                    dialog.reset(folder.clone(), sources.clone(), window, cx)
                });
                existing.clone()
            }
            None => {
                let created = cx
                    .new(|cx| BatchRenameDialog::new(folder.clone(), sources.clone(), window, cx));
                self.batch_rename = Some(created.clone());
                created
            }
        };
        let focus = dialog.read(cx).rules[0]
            .find
            .read(cx)
            .focus_handle(cx)
            .clone();
        let browser = cx.weak_entity();
        window.open_dialog(cx, move |modal, window, cx| {
            window.focus(&focus, cx);
            let viewport = window.viewport_size();
            let geometry = dialog_geometry(f32::from(viewport.width), f32::from(viewport.height));
            let dialog_for_close = dialog.clone();
            let dialog_for_ok = dialog.clone();
            let browser_for_ok = browser.clone();
            modal
                .w(px(geometry.body_width))
                .margin_top(px(geometry.margin_top))
                .p_0()
                .close_button(false)
                .keyboard(false)
                .overlay_closable(false)
                .on_close(move |_, _, cx| {
                    dialog_for_close.update(cx, |dialog, cx| {
                        if !dialog.applying {
                            dialog.generation = dialog.generation.wrapping_add(1);
                            dialog.preview_task.take();
                            dialog.recovery_paths = Arc::default();
                            dialog.release_existing(cx);
                            dialog.clear_preview(cx);
                        }
                    });
                })
                .child(
                    div()
                        .h(px(geometry.body_height))
                        .min_h_0()
                        .overflow_hidden()
                        .child(dialog.clone()),
                )
                .on_ok(move |_, window, cx| {
                    let plan = dialog_for_ok.update(cx, |dialog, cx| {
                        if dialog.applying || dialog.fatal || dialog.checking {
                            return None;
                        }
                        let plan = dialog.plan.take()?;
                        if !plan.can_apply() {
                            dialog.plan = Some(plan);
                            return None;
                        }
                        dialog.applying = true;
                        dialog.error = None;
                        cx.notify();
                        Some(plan)
                    });
                    let Some(plan) = plan else {
                        return false;
                    };
                    if let Some(browser) = browser_for_ok.upgrade() {
                        browser.update(cx, |browser, cx| {
                            let folder = dialog_for_ok.read(cx).folder.clone();
                            browser.start_batch_rename(plan, folder, window, cx);
                        });
                    }
                    false
                })
        });
        cx.notify();
    }

    fn start_batch_rename(
        &mut self,
        plan: Arc<BatchRenamePlan>,
        folder: Entity<Folder>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dialog = self.batch_rename.clone();
        if folder.read(cx).is_busy() {
            if let Some(dialog) = &dialog {
                let retry_plan = plan.clone();
                dialog.update(cx, |dialog, cx| {
                    dialog.applying = false;
                    dialog.plan = Some(retry_plan);
                    dialog.error = Some(SharedString::from(
                        "Wait for the folder listing to finish before renaming.",
                    ));
                    cx.notify();
                });
            }
            return;
        }
        let fs = cx.global::<FerretState>().fs.clone();
        let thumbs = cx.global::<FerretState>().rendered_thumbs.clone();
        let window_handle = window.window_handle();
        let cursor = folder.read(cx).cursor_path();
        folder.update(cx, |folder, _| folder.suspend_watch());

        cx.spawn(async move |_, cx| {
            let prepared = cx
                .background_executor()
                .spawn(async move {
                    let changed_sources = plan.changed_source_paths();
                    thumbs.remove_many(&changed_sources);
                    let result = match ferret_ops::apply(&fs, &plan).await {
                        Ok(renamed) => Ok(renamed),
                        Err(error) => {
                            let kind = match &error {
                                BatchRenameError::InvalidPlan { .. } => {
                                    RenameFailureKind::InvalidPlan
                                }
                                BatchRenameError::RecoveryRequired { .. } => {
                                    RenameFailureKind::RecoveryRequired
                                }
                                _ => RenameFailureKind::Other,
                            };
                            let recovery_paths = match &error {
                                BatchRenameError::RecoveryRequired { recovery_paths, .. } => {
                                    Arc::from(recovery_paths.as_slice())
                                }
                                _ => Arc::default(),
                            };
                            Err(RenameFailure {
                                retry_plan: matches!(&kind, RenameFailureKind::Other)
                                    .then(|| plan.clone()),
                                kind,
                                message: SharedString::from(error.to_string()),
                                recovery_paths,
                            })
                        }
                    };
                    for row in plan.rows() {
                        fs.invalidate(&row.source);
                        fs.invalidate(&row.destination);
                    }
                    let renames = plan
                        .renames()
                        .map(|(source, destination)| (source.clone(), destination.clone()))
                        .collect();
                    let selection = plan.destination_paths().into_iter().collect::<HashSet<_>>();
                    let cursor = cursor
                        .as_ref()
                        .and_then(|source| plan.destination_for(source))
                        .or_else(|| plan.rows().first().map(|row| row.destination.clone()));
                    PreparedRename {
                        result,
                        renames,
                        selection,
                        cursor,
                    }
                })
                .await;

            match prepared.result {
                Ok(renamed) => {
                    folder.update(cx, |folder, cx| {
                        let _ = folder.apply_renames(&prepared.renames, cx);
                        folder.reload_with_mapped_selection(
                            prepared.selection.clone(),
                            prepared.cursor.clone(),
                            &prepared.renames,
                            cx,
                        );
                    });
                    let _ = window_handle.update(cx, |_, window, cx| {
                        if let Some(dialog) = &dialog {
                            dialog.update(cx, |dialog, cx| {
                                dialog.applying = false;
                                dialog.recovery_paths = Arc::default();
                                dialog.release_existing(cx);
                                dialog.clear_preview(cx);
                                cx.notify();
                            });
                        }
                        window.close_dialog(cx);
                        window.push_notification(
                            Notification::success(format!("Renamed {renamed} items.")),
                            cx,
                        );
                    });
                }
                Err(failure) => {
                    let recovery_required =
                        matches!(&failure.kind, RenameFailureKind::RecoveryRequired);
                    folder.update(cx, |folder, cx| {
                        folder.reload_preserving_state(cx);
                    });
                    let invalid_plan = matches!(&failure.kind, RenameFailureKind::InvalidPlan);
                    let message = failure.message;
                    let retry_plan = failure.retry_plan;
                    let recovery_paths = failure.recovery_paths;
                    let _ = window_handle.update(cx, |_, window, cx| {
                        if let Some(dialog) = &dialog {
                            dialog.update(cx, |dialog, cx| {
                                dialog.applying = false;
                                dialog.fatal = recovery_required;
                                dialog.recovery_paths = recovery_paths;
                                if invalid_plan {
                                    dialog.load_existing(cx);
                                } else {
                                    dialog.plan = retry_plan;
                                    dialog.error = Some(message.clone());
                                    cx.notify();
                                }
                            });
                        }
                        if recovery_required {
                            window.push_notification(Notification::error(message), cx);
                        }
                    });
                }
            }
        })
        .detach();
    }
}
