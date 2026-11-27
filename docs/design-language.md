# Chatter design language (desktop and tablet)

Set by the owner and cofounder designer on 2026-11-27, starting from the section-card sidebar (release 60). Every screen follows these rules. Phone layouts keep their own spacing where noted. Mockups live in the PR description of client #74.

## Page and containers

- **Page:** pure OLED black (`Colors.background`) in dark mode. No ambient tinted wash on desktop or tablet.
- **Containers:** content sits in framed containers on the black page. A container has:
  - radius `Spacing.radius-panel` (12 px);
  - a 1 px `Colors.border` rim;
  - an inset of `Spacing.panel-gutter` (8 px) from the window edge and from its neighbours.

  Examples: the chat column, the thread panel, details panes and settings pages. Inside, a container stays black unless it is a card.
- **Cards:** grouped content inside a sidebar or panel, such as section cards, settings groups, member lists and the pinned bar. A card has:
  - fill `Colors.surface-card`;
  - radius `Spacing.radius-panel`;
  - `Spacing.sm` inner padding;
  - an 8 px gap between cards.

  Cards never touch a container edge; they keep the gutter.
- **Radius ladder:** window `radius-window` (18) → containers and cards `radius-panel` (12) → controls inside cards `radius-control` (8) → pills and fully rounded chips `radius-full`. Inner radius = outer radius − gap. Message bubbles keep `radius-bubble` / `radius-bubble-joint`.
- **Floating chrome:** a header island or composer island inside a container sits one gutter (8 px) in from the container edge on every side. It is a `GlassSurface` with the same rim. No full-width flat slabs, and no square-cornered bars that span a container edge to edge.

## Type and labels

- Card or group headers: 11 px uppercase, letter-spacing 0.06 em, `Colors.text-muted`, semibold. Emoji are not uppercased.
- Titles in a header island: 14 px semibold. Subtitles: 11 px muted.
- Row title: 13 px semibold. Row preview: 12 px muted.

## Colour

- **Accent** (`Colors.primary`, teal) is reserved for:
  - the primary action on a screen;
  - the selected state (row fill at `tint-strong`, focus ring);
  - unread badges;
  - links.
- It is **not** used for decorative stripes, labels or eyebrows. In particular, no coloured left "accent rail" on rounded cards. A quoted, pinned or replied-to message shows its sender name in `text-secondary` with a neutral 2 px rule in `Colors.border`, or no rule at all.
- **Muted states** use neutral fills (`surface-control`), never accent.
- **No literal colours** in components. Use theme tokens only.

## Controls

- Buttons come only from the shared families: `Button`, `HeaderButton`, `IconAction` (enforced by the `screens_do_not_define_private_button_families` test).
- Text inputs: `radius-control` (8), `surface-control` fill, no inner bars. A composer is ONE island holding emoji, field, attach and send. Reply, edit and attachment previews sit inside the island above the field, never as a separate slab.
- Empty states: centred icon, 14 px title, 12 px muted line, at most one primary button.

## Lists and rows

- Selected row: `tint-strong` fill, radius 8, inset inside its card.
- Selected state follows the shape: round avatars get an accent ring; square or rounded tiles (Saved, rail actions, icon tiles) get a `tint-strong` fill in their own radius, never a round ring drawn inside a square tile.
- Avatars: circles. A missing avatar shows initials on a token colour, never a black disc.
