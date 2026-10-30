//! Media ordering for a message's album, independent of other attachments.

/// Return each attachment's media position (-1 for nonmedia) and the media total.
pub fn album_positions(attachments: impl IntoIterator<Item = (bool, bool)>) -> (Vec<i32>, i32) {
    let mut positions = Vec::new();
    let mut count = 0;
    for (is_image, is_video) in attachments {
        if is_image || is_video {
            positions.push(count);
            count += 1;
        } else {
            positions.push(-1);
        }
    }
    (positions, count)
}

/// Positions in the original attachment list that need a row below the grid.
pub fn standalone_indices(positions: &[i32], media_count: i32) -> Vec<i32> {
    positions
        .iter()
        .enumerate()
        .filter_map(|(index, position)| {
            (media_count < 2 || *position < 0).then_some(index as i32)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::album_positions;

    #[test]
    fn mixed_attachments_keep_media_order() {
        let (positions, count) = album_positions([
            (false, false), (true, false), (false, true), (false, false), (true, false),
        ]);
        assert_eq!(positions, [-1, 0, 1, -1, 2]);
        assert_eq!(count, 3);
    }
}
