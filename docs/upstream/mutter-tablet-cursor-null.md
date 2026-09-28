# Mutter: NULL cursor dereference with a secondary tablet cursor (draft)

Draft report for GNOME Mutter; not yet filed.

## Summary

GNOME Shell crashes with SIGSEGV in `clutter_cursor_realize_texture` when a
tablet tool has a secondary cursor renderer whose cursor is unset while a
fullscreen window is a direct-scanout candidate.

## Versions

- mutter 50.4 (Fedora 44, `mutter-50.4-1.fc44`)
- gnome-shell 50.4
- Wayland session, native backend

## Stack

```
#0 clutter_cursor_realize_texture        (libmutter-clutter-18.so.0 + 0x44b84)
#1 meta_cursor_renderer_calculate_rect
#2 has_overlapping_cursor_overlay_foreach
#3 meta_clutter_backend_native_foreach_sprite
#4 meta_compositor_native_before_paint
```

`rdi` is `0x0` at the faulting `mov (%rdi),%rax`: the cursor argument is NULL.

## Cause

- `meta_seat_native_maybe_ensure_cursor_renderer_for_sprite` gives every
  sprite other than the one owning the native renderer a secondary
  `MetaCursorRenderer`.
- The base class `meta_cursor_renderer_real_update_cursor` returns `TRUE`
  unconditionally, so `needs_overlay` becomes `TRUE` even when `cursor` is
  NULL; `update_stage_overlay` then stores the NULL as `overlay_cursor`.
- A tablet tool's cursor is NULL whenever `cursor_source` is
  `CURSOR_SOURCE_UNSET` (`meta_wayland_tablet_tool_get_cursor`), e.g. after
  entering a surface whose client never called `set_cursor`.
- `find_scanout_candidate` → `surface_actor_has_overlapping_cursor_overlays`
  → `has_overlapping_cursor_overlay_foreach` checks only
  `meta_cursor_renderer_needs_overlay` and passes the NULL cursor to
  `meta_cursor_renderer_calculate_rect`, which dereferences it.

## Reproduction

1. Create a virtual pen tablet with uinput (absolute X/Y, `BTN_TOOL_PEN`)
   alongside a regular mouse.
2. Bring the pen into proximity over a fullscreen window eligible for direct
   scanout (e.g. a fullscreen video).
3. Move the pen; GNOME Shell crashes.

With `MUTTER_DEBUG_PAINT=disable-direct-scanout` the crash does not occur.

## Suggested fix

Either of:

```c
static gboolean
meta_cursor_renderer_real_update_cursor (MetaCursorRenderer *renderer,
                                         ClutterCursor      *cursor)
{
  if (!cursor)
    return FALSE;

  clutter_cursor_realize_texture (cursor);
  return TRUE;
}
```

or guard the overlap check:

```c
  cursor = meta_cursor_renderer_get_cursor (cursor_renderer);
  if (!cursor)
    return TRUE;
```
