// SPDX-License-Identifier: GPL-3.0-or-later

mod xdg_shell;
mod xwayland;

use std::{mem, os::fd::OwnedFd, time::Duration};

use smithay::{
    backend::renderer::utils::{self, with_renderer_surface_state},
    delegate_compositor, delegate_data_control, delegate_data_device, delegate_fractional_scale,
    delegate_layer_shell, delegate_output, delegate_presentation, delegate_primary_selection,
    delegate_relative_pointer, delegate_seat, delegate_shm, delegate_viewporter,
    desktop::{
        self, find_popup_root_surface, get_popup_toplevel_coords, layer_map_for_output,
        utils::surface_primary_scanout_output, PopupKind, WindowSurfaceType,
    },
    input::{pointer::CursorImageStatus, Seat, SeatHandler, SeatState},
    output::Output,
    reexports::{
        calloop::Interest,
        wayland_protocols::xdg::shell::server::xdg_positioner::ConstraintAdjustment,
        wayland_server::{
            protocol::{
                wl_buffer::WlBuffer, wl_data_source::WlDataSource, wl_output::WlOutput,
                wl_surface::WlSurface,
            },
            Client, Resource,
        },
    },
    utils::{Logical, Rectangle, SERIAL_COUNTER},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            self, BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
            SurfaceAttributes,
        },
        dmabuf,
        fractional_scale::{self, FractionalScaleHandler},
        output::OutputHandler,
        seat::WaylandFocus,
        selection::{
            data_device::{
                set_data_device_focus, ClientDndGrabHandler, DataDeviceHandler, DataDeviceState,
                ServerDndGrabHandler,
            },
            primary_selection::{
                set_primary_focus, PrimarySelectionHandler, PrimarySelectionState,
            },
            wlr_data_control::{DataControlHandler, DataControlState},
            SelectionHandler, SelectionSource, SelectionTarget,
        },
        shell::{
            wlr_layer::{self, Layer, LayerSurfaceData, WlrLayerShellHandler, WlrLayerShellState},
            xdg::{PopupSurface, XdgPopupSurfaceData, XdgToplevelSurfaceData},
        },
        shm::{ShmHandler, ShmState},
    },
    xwayland::{X11Wm, XWaylandClientData},
};
use tracing::{error, trace, warn};

use crate::{
    backend::Backend,
    delegate_gamma_control, delegate_screencopy,
    focus::{keyboard::KeyboardFocusTarget, pointer::PointerFocusTarget},
    protocol::{
        gamma_control::{GammaControlHandler, GammaControlManagerState},
        screencopy::{Screencopy, ScreencopyHandler},
    },
    state::{ClientState, Pinnacle, State, WithState},
};

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.pinnacle.compositor_state
    }

    fn new_surface(&mut self, surface: &WlSurface) {
        compositor::add_pre_commit_hook::<Self, _>(surface, |state, _display_handle, surface| {
            let maybe_dmabuf = compositor::with_states(surface, |surface_data| {
                surface_data
                    .cached_state
                    .pending::<SurfaceAttributes>()
                    .buffer
                    .as_ref()
                    .and_then(|assignment| match assignment {
                        BufferAssignment::NewBuffer(buffer) => dmabuf::get_dmabuf(buffer).ok(),
                        _ => None,
                    })
            });
            if let Some(dmabuf) = maybe_dmabuf {
                if let Ok((blocker, source)) = dmabuf.generate_blocker(Interest::READ) {
                    let client = surface
                        .client()
                        .expect("Surface has no client/is no longer alive");
                    let res =
                        state
                            .pinnacle
                            .loop_handle
                            .insert_source(source, move |_, _, state| {
                                state
                                    .client_compositor_state(&client)
                                    .blocker_cleared(state, &state.pinnacle.display_handle.clone());
                                Ok(())
                            });
                    if res.is_ok() {
                        compositor::add_blocker(surface, blocker);
                    }
                }
            }
        });
    }

    fn commit(&mut self, surface: &WlSurface) {
        trace!("commit on surface {surface:?}");

        utils::on_commit_buffer_handler::<State>(surface);

        X11Wm::commit_hook::<State>(surface);

        self.backend.early_import(surface);

        let mut root = surface.clone();
        while let Some(parent) = compositor::get_parent(&root) {
            root = parent;
        }

        if !compositor::is_sync_subsurface(surface) {
            if let Some(window) = self.pinnacle.window_for_surface(&root) {
                window.on_commit();
                if let Some(loc) = window.with_state_mut(|state| state.target_loc.take()) {
                    self.pinnacle.space.map_element(window.clone(), loc, false);
                }
            }
        };

        self.pinnacle.popup_manager.commit(surface);

        if let Some(new_window) = self
            .pinnacle
            .new_windows
            .iter()
            .find(|win| win.wl_surface().as_ref() == Some(surface))
            .cloned()
        {
            let Some(is_mapped) =
                with_renderer_surface_state(surface, |state| state.buffer().is_some())
            else {
                unreachable!("on_commit_buffer_handler was called previously");
            };

            if is_mapped {
                self.pinnacle.new_windows.retain(|win| win != &new_window);
                self.pinnacle.windows.push(new_window.clone());

                if let Some(output) = self.pinnacle.focused_output() {
                    tracing::debug!("Placing toplevel");
                    new_window.place_on_output(output);
                    output.with_state_mut(|state| state.focus_stack.set_focus(new_window.clone()));
                }

                // FIXME: I'm mapping way offscreen here then sending a frame to prevent a window from
                // |      mapping with its default geometry then immediately resizing
                // |      because I don't set a target geometry before the initial configure.
                self.pinnacle
                    .space
                    .map_element(new_window.clone(), (1000000, 0), true);

                self.pinnacle.raise_window(new_window.clone(), true);

                self.pinnacle.apply_window_rules(&new_window);

                if let Some(focused_output) = self.pinnacle.focused_output().cloned() {
                    self.pinnacle.request_layout(&focused_output);
                    new_window.send_frame(
                        &focused_output,
                        self.pinnacle.clock.now(),
                        Some(Duration::ZERO),
                        surface_primary_scanout_output,
                    );
                }

                self.pinnacle.loop_handle.insert_idle(move |state| {
                    state
                        .pinnacle
                        .seat
                        .get_keyboard()
                        .expect("Seat had no keyboard") // FIXME: actually handle error
                        .set_focus(
                            state,
                            Some(KeyboardFocusTarget::Window(new_window)),
                            SERIAL_COUNTER.next_serial(),
                        );
                });
            } else if new_window.toplevel().is_some() {
                new_window.on_commit();
                self.pinnacle.ensure_initial_configure(surface);
            }

            return;
        }

        self.pinnacle.ensure_initial_configure(surface);

        self.pinnacle.move_surface_if_resized(surface);

        let outputs = if let Some(window) = self.pinnacle.window_for_surface(surface) {
            let mut outputs = self.pinnacle.space.outputs_for_element(&window);

            // When the window hasn't been mapped `outputs` is empty,
            // so also trigger a render using the window's tags' output
            if let Some(output) = window.output(&self.pinnacle) {
                outputs.push(output);
            }
            outputs // surface is a window
        } else if let Some(window) = self.pinnacle.window_for_surface(&root) {
            let mut outputs = self.pinnacle.space.outputs_for_element(&window);
            if let Some(output) = window.output(&self.pinnacle) {
                outputs.push(output);
            }
            outputs // surface is a root window
        } else if let Some(PopupKind::Xdg(surf)) = self.pinnacle.popup_manager.find_popup(surface) {
            let geo = surf.with_pending_state(|state| state.geometry);
            let outputs = self
                .pinnacle
                .space
                .outputs()
                .filter_map(|output| {
                    let op_geo = self.pinnacle.space.output_geometry(output);
                    op_geo.and_then(|op_geo| op_geo.overlaps_or_touches(geo).then_some(output))
                })
                .cloned()
                .collect::<Vec<_>>();
            outputs
        } else if let Some(output) = self
            .pinnacle
            .space
            .outputs()
            .find(|op| {
                let layer_map = layer_map_for_output(op);
                layer_map
                    .layer_for_surface(surface, WindowSurfaceType::ALL)
                    .is_some()
            })
            .cloned()
        {
            vec![output] // surface is a layer surface
        } else {
            return;
        };

        for output in outputs {
            self.schedule_render(&output);
        }
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        if let Some(state) = client.get_data::<XWaylandClientData>() {
            return &state.compositor_state;
        }
        if let Some(state) = client.get_data::<ClientState>() {
            return &state.compositor_state;
        }
        panic!("Unknown client data type");
    }
}
delegate_compositor!(State);

impl Pinnacle {
    fn ensure_initial_configure(&mut self, surface: &WlSurface) {
        if let (Some(window), _) | (None, Some(window)) = (
            self.window_for_surface(surface),
            self.new_window_for_surface(surface),
        ) {
            if let Some(toplevel) = window.toplevel() {
                let initial_configure_sent = compositor::with_states(surface, |states| {
                    states
                        .data_map
                        .get::<XdgToplevelSurfaceData>()
                        .expect("XdgToplevelSurfaceData wasn't in surface's data map")
                        .lock()
                        .expect("Failed to lock Mutex<XdgToplevelSurfaceData>")
                        .initial_configure_sent
                });

                if !initial_configure_sent {
                    tracing::debug!("Initial configure");
                    toplevel.send_configure();
                }
            }
            return;
        }

        if let Some(popup) = self.popup_manager.find_popup(surface) {
            let PopupKind::Xdg(popup) = &popup else { return };
            let initial_configure_sent = compositor::with_states(surface, |states| {
                states
                    .data_map
                    .get::<XdgPopupSurfaceData>()
                    .expect("XdgPopupSurfaceData wasn't in popup's data map")
                    .lock()
                    .expect("Failed to lock Mutex<XdgPopupSurfaceData>")
                    .initial_configure_sent
            });
            if !initial_configure_sent {
                popup
                    .send_configure()
                    .expect("popup initial configure failed");
            }
            return;
        }

        if let Some(output) = self.space.outputs().find(|op| {
            let map = layer_map_for_output(op);
            map.layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
                .is_some()
        }) {
            layer_map_for_output(output).arrange();

            let initial_configure_sent = compositor::with_states(surface, |states| {
                states
                    .data_map
                    .get::<LayerSurfaceData>()
                    .expect("no LayerSurfaceData")
                    .lock()
                    .expect("failed to lock data")
                    .initial_configure_sent
            });

            if !initial_configure_sent {
                layer_map_for_output(output)
                    .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
                    .expect("no layer for surface")
                    .layer_surface()
                    .send_configure();
            }
        }
    }
}

impl ClientDndGrabHandler for State {
    fn started(
        &mut self,
        _source: Option<WlDataSource>,
        icon: Option<WlSurface>,
        _seat: Seat<Self>,
    ) {
        self.pinnacle.dnd_icon = icon;
    }

    fn dropped(&mut self, _seat: Seat<Self>) {
        self.pinnacle.dnd_icon = None;
    }
}

impl ServerDndGrabHandler for State {}

impl SelectionHandler for State {
    type SelectionUserData = ();

    fn new_selection(
        &mut self,
        ty: SelectionTarget,
        source: Option<SelectionSource>,
        _seat: Seat<Self>,
    ) {
        if let Some(xwm) = self.pinnacle.xwm.as_mut() {
            if let Err(err) = xwm.new_selection(ty, source.map(|source| source.mime_types())) {
                tracing::warn!(?err, ?ty, "Failed to set Xwayland selection");
            }
        }
    }

    fn send_selection(
        &mut self,
        ty: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
        _seat: Seat<Self>,
        _user_data: &(),
    ) {
        if let Some(xwm) = self.pinnacle.xwm.as_mut() {
            if let Err(err) =
                xwm.send_selection(ty, mime_type, fd, self.pinnacle.loop_handle.clone())
            {
                tracing::warn!(?err, "Failed to send primary (X11 -> Wayland)");
            }
        }
    }
}

impl DataDeviceHandler for State {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.pinnacle.data_device_state
    }
}
delegate_data_device!(State);

impl PrimarySelectionHandler for State {
    fn primary_selection_state(&self) -> &PrimarySelectionState {
        &self.pinnacle.primary_selection_state
    }
}
delegate_primary_selection!(State);

impl DataControlHandler for State {
    fn data_control_state(&self) -> &DataControlState {
        &self.pinnacle.data_control_state
    }
}
delegate_data_control!(State);

impl SeatHandler for State {
    type KeyboardFocus = KeyboardFocusTarget;
    type PointerFocus = PointerFocusTarget;
    type TouchFocus = PointerFocusTarget;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.pinnacle.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.pinnacle.cursor_status = image;
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&Self::KeyboardFocus>) {
        let focus_client = focused.and_then(|foc_target| {
            self.pinnacle
                .display_handle
                .get_client(foc_target.wl_surface()?.id())
                .ok()
        });
        set_data_device_focus(&self.pinnacle.display_handle, seat, focus_client.clone());
        set_primary_focus(&self.pinnacle.display_handle, seat, focus_client);
    }
}
delegate_seat!(State);

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.pinnacle.shm_state
    }
}
delegate_shm!(State);

impl OutputHandler for State {}
delegate_output!(State);

delegate_viewporter!(State);

impl FractionalScaleHandler for State {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        // comment yanked from anvil
        // Here we can set the initial fractional scale
        //
        // First we look if the surface already has a primary scan-out output, if not
        // we test if the surface is a subsurface and try to use the primary scan-out output
        // of the root surface. If the root also has no primary scan-out output we just try
        // to use the first output of the toplevel.
        // If the surface is the root we also try to use the first output of the toplevel.
        //
        // If all the above tests do not lead to a output we just use the first output
        // of the space (which in case of anvil will also be the output a toplevel will
        // initially be placed on)
        let mut root = surface.clone();
        while let Some(parent) = compositor::get_parent(&root) {
            root = parent;
        }

        compositor::with_states(&surface, |states| {
            let primary_scanout_output =
                desktop::utils::surface_primary_scanout_output(&surface, states)
                    .or_else(|| {
                        if root != surface {
                            compositor::with_states(&root, |states| {
                                desktop::utils::surface_primary_scanout_output(&root, states)
                                    .or_else(|| {
                                        self.pinnacle.window_for_surface(&root).and_then(|window| {
                                            self.pinnacle
                                                .space
                                                .outputs_for_element(&window)
                                                .first()
                                                .cloned()
                                        })
                                    })
                            })
                        } else {
                            self.pinnacle.window_for_surface(&root).and_then(|window| {
                                self.pinnacle
                                    .space
                                    .outputs_for_element(&window)
                                    .first()
                                    .cloned()
                            })
                        }
                    })
                    .or_else(|| self.pinnacle.space.outputs().next().cloned());
            if let Some(output) = primary_scanout_output {
                fractional_scale::with_fractional_scale(states, |fractional_scale| {
                    fractional_scale.set_preferred_scale(output.current_scale().fractional_scale());
                });
            }
        });
    }
}

delegate_fractional_scale!(State);

delegate_relative_pointer!(State);

delegate_presentation!(State);

impl WlrLayerShellHandler for State {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.pinnacle.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: wlr_layer::LayerSurface,
        output: Option<WlOutput>,
        _layer: Layer,
        namespace: String,
    ) {
        tracing::debug!("New layer surface");
        let output = output
            .as_ref()
            .and_then(Output::from_resource)
            .or_else(|| self.pinnacle.space.outputs().next().cloned());

        let Some(output) = output else {
            error!("New layer surface, but there was no output to map it on");
            return;
        };

        if let Err(err) =
            layer_map_for_output(&output).map_layer(&desktop::LayerSurface::new(surface, namespace))
        {
            error!("Failed to map layer surface: {err}");
        }

        self.pinnacle.loop_handle.insert_idle(move |state| {
            state.pinnacle.request_layout(&output);
        });
    }

    fn layer_destroyed(&mut self, surface: wlr_layer::LayerSurface) {
        let mut output: Option<Output> = None;
        if let Some((mut map, layer, op)) = self.pinnacle.space.outputs().find_map(|o| {
            let map = layer_map_for_output(o);
            let layer = map
                .layers()
                .find(|&layer| layer.layer_surface() == &surface)
                .cloned();
            layer.map(|layer| (map, layer, o))
        }) {
            map.unmap_layer(&layer);
            output = Some(op.clone());
        }

        if let Some(output) = output {
            self.pinnacle.loop_handle.insert_idle(move |state| {
                state.pinnacle.request_layout(&output);
            });
        }
    }

    fn new_popup(&mut self, _parent: wlr_layer::LayerSurface, popup: PopupSurface) {
        trace!("WlrLayerShellHandler::new_popup");
        self.pinnacle.position_popup(&popup);
    }
}
delegate_layer_shell!(State);

impl ScreencopyHandler for State {
    fn frame(&mut self, frame: Screencopy) {
        let output = frame.output().clone();
        if !frame.with_damage() {
            self.schedule_render(&output);
        }
        output.with_state_mut(|state| state.screencopy.replace(frame));
    }
}
delegate_screencopy!(State);

impl GammaControlHandler for State {
    fn gamma_control_manager_state(&mut self) -> &mut GammaControlManagerState {
        &mut self.pinnacle.gamma_control_manager_state
    }

    fn get_gamma_size(&mut self, output: &Output) -> Option<u32> {
        let Backend::Udev(udev) = &self.backend else {
            return None;
        };

        match udev.gamma_size(output) {
            Ok(0) => None, // Setting gamma is not supported
            Ok(size) => Some(size),
            Err(err) => {
                warn!(
                    "Failed to get gamma size for output {}: {err}",
                    output.name()
                );
                None
            }
        }
    }

    fn set_gamma(&mut self, output: &Output, gammas: [&[u16]; 3]) -> bool {
        let Backend::Udev(udev) = &mut self.backend else {
            warn!("Setting gamma is not supported on the winit backend");
            return false;
        };

        match udev.set_gamma(output, Some(gammas)) {
            Ok(_) => true,
            Err(err) => {
                warn!("Failed to set gamma for output {}: {err}", output.name());
                false
            }
        }
    }

    fn gamma_control_destroyed(&mut self, output: &Output) {
        let Backend::Udev(udev) = &mut self.backend else {
            warn!("Resetting gamma is not supported on the winit backend");
            return;
        };

        if let Err(err) = udev.set_gamma(output, None) {
            warn!("Failed to set gamma for output {}: {err}", output.name());
        }
    }
}
delegate_gamma_control!(State);

impl Pinnacle {
    fn position_popup(&self, popup: &PopupSurface) {
        trace!("State::position_popup");
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };

        let mut positioner = popup.with_pending_state(|state| mem::take(&mut state.positioner));

        let popup_geo = (|| -> Option<Rectangle<i32, Logical>> {
            let parent = popup.get_parent_surface()?;

            if parent == root {
                // Slide toplevel popup x's instead of flipping; this mimics Awesome
                positioner
                    .constraint_adjustment
                    .remove(ConstraintAdjustment::FlipX);
            }

            let (root_global_loc, output) = if let Some(win) = self.window_for_surface(&root) {
                let win_geo = self.space.element_geometry(&win)?;
                (win_geo.loc, self.focused_output()?.clone())
            } else {
                self.space.outputs().find_map(|op| {
                    let layer_map = layer_map_for_output(op);
                    let layer = layer_map.layer_for_surface(&root, WindowSurfaceType::TOPLEVEL)?;
                    let output_loc = self.space.output_geometry(op)?.loc;
                    Some((
                        layer_map.layer_geometry(layer)?.loc + output_loc,
                        op.clone(),
                    ))
                })?
            };

            let parent_global_loc = if root == parent {
                root_global_loc
            } else {
                root_global_loc + get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()))
            };

            let mut output_geo = self.space.output_geometry(&output)?;

            // Make local to parent
            output_geo.loc -= parent_global_loc;
            Some(positioner.get_unconstrained_geometry(output_geo))
        })()
        .unwrap_or_else(|| positioner.get_geometry());

        popup.with_pending_state(|state| {
            state.geometry = popup_geo;
            state.positioner = positioner;
        });
    }
}
