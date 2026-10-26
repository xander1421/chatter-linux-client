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
