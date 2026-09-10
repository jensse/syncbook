"""
Worked example: add a native ink annotation (a circle) to a reMarkable page,
on its own layer, without disturbing the original content.

Run via uv (installs rmscene on the fly, no venv setup needed):

    uv run --with rmscene python3 examples/annotate.py page.rm page-annotated.rm

Then push it:

    syncbook pushrm "My Notebook" <page-uuid> page-annotated.rm

See the README's "Advanced: writing native ink annotations" section for why
this needs left_id = right_id = CrdtId(0, 0) on the new layer's registration,
not a reference to an existing item -- getting that wrong is silently
accepted by rmscene's reader but rejected by the real reMarkable app.

You will need to change cx/cy/rx/ry below to match where you actually want
to draw -- this is a worked example, not a general annotation tool. A rough
way to find coordinates: render the original page (rmc + cairosvg, same as
`syncbook pullrm` does) to a 1404x1872 PNG, find your target's pixel
position in an image viewer, then convert with rm_x = png_x - 702,
rm_y = png_y (empirically verified against an existing stroke's known
on-page position; reMarkable's coordinate origin is horizontally centered).
"""

import io
import math
import sys
import uuid

from rmscene import scene_items as si
from rmscene import scene_stream as ss

LAYER_NAME = "Claude"

# Circle center/radii in reMarkable page coordinates -- adjust per use.
CX, CY = -120.0, 225.0
RX, RY = 190.0, 100.0
N_POINTS = 72


def build_circle_points(cx: float, cy: float, rx: float, ry: float, n: int) -> list[si.Point]:
    points = []
    for i in range(n + 1):
        theta = 2 * math.pi * i / n
        points.append(
            si.Point(
                x=cx + rx * math.cos(theta),
                y=cy + ry * math.sin(theta),
                speed=0,
                direction=0,
                width=12,
                pressure=0,
            )
        )
    return points


def main(src: str, dst: str) -> None:
    with open(src, "rb") as f:
        blocks = list(ss.read_blocks(f))

    author_block = next(b for b in blocks if isinstance(b, ss.AuthorIdsBlock))
    new_author_idx = max(author_block.author_uuids.keys()) + 1
    new_author_uuids = dict(author_block.author_uuids)
    new_author_uuids[new_author_idx] = uuid.uuid4()
    new_author_block = ss.AuthorIdsBlock(author_uuids=new_author_uuids)

    layer_node = ss.CrdtId(new_author_idx, 1)
    layer_label_ts = ss.CrdtId(new_author_idx, 2)
    layer_reg_item = ss.CrdtId(new_author_idx, 3)
    line_item = ss.CrdtId(new_author_idx, 4)

    # SceneTreeBlock must precede TreeNodeBlock -- xochitl's tree builder
    # creates the node on SceneTreeBlock and only *updates* it (label,
    # visibility) on TreeNodeBlock; the reverse order fails with
    # "Node does not exist for TreeNodeBlock".
    new_scene_tree_block = ss.SceneTreeBlock(
        extra_data=b"",
        tree_id=layer_node,
        node_id=ss.CrdtId(0, 0),
        is_update=True,
        parent_id=ss.CrdtId(0, 1),
    )

    new_tree_node = ss.TreeNodeBlock(
        extra_data=b"",
        group=si.Group(
            node_id=layer_node,
            children=si.CrdtSequence(),
            label=si.LwwValue(timestamp=layer_label_ts, value=LAYER_NAME),
            visible=si.LwwValue(timestamp=ss.CrdtId(0, 0), value=True),
        ),
    )

    # left_id/right_id = (0, 0): an independently-added, causally
    # unconnected sibling. Do NOT point left_id at an existing item's ID --
    # see the module docstring and README.
    new_group_registration = ss.SceneGroupItemBlock(
        extra_data=b"",
        parent_id=ss.CrdtId(0, 1),
        item=ss.CrdtSequenceItem(
            item_id=layer_reg_item,
            left_id=ss.CrdtId(0, 0),
            right_id=ss.CrdtId(0, 0),
            deleted_length=0,
            value=layer_node,
        ),
        extra_value_data=b"",
    )

    new_line = si.Line(
        color=si.PenColor.GRAY,  # e-ink displays are grayscale, not color
        tool=si.Pen.FINELINER_2,
        points=build_circle_points(CX, CY, RX, RY, N_POINTS),
        thickness_scale=1.5,
        starting_length=0.0,
        move_id=None,
        color_rgba=None,
    )

    new_line_block = ss.SceneLineItemBlock(
        extra_data=b"",
        parent_id=layer_node,
        item=ss.CrdtSequenceItem(
            item_id=line_item,
            left_id=ss.CrdtId(0, 0),
            right_id=ss.CrdtId(0, 0),
            deleted_length=0,
            value=new_line,
        ),
        extra_value_data=b"",
    )

    new_blocks = [new_author_block if isinstance(b, ss.AuthorIdsBlock) else b for b in blocks]
    new_blocks += [new_scene_tree_block, new_tree_node, new_group_registration, new_line_block]

    buf = io.BytesIO()
    ss.write_blocks(buf, new_blocks)
    with open(dst, "wb") as f:
        f.write(buf.getvalue())

    print(f"wrote {dst} (new layer '{LAYER_NAME}', author index {new_author_idx})")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        print(f"usage: {sys.argv[0]} <source.rm> <dest.rm>", file=sys.stderr)
        sys.exit(1)
    main(sys.argv[1], sys.argv[2])
