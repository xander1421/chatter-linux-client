use super::*;

use super::super::repo_utils::{
    parse_datetime, require_array_32, require_array_64, vec_to_array_32,
};

pub(super) fn row_to_group(row: &rusqlite::Row<'_>) -> rusqlite::Result<AnonGroup> {
    use crate::domain::anon_group::AnonGroupStatus;

    let id_str: String = row.get(0)?;
    let id = id_str.parse().map_err(|_| rusqlite::Error::InvalidQuery)?;

    // Status column may not exist in older schemas, default to Active
    let status: i32 = row.get(6).unwrap_or(0);

    let owner_id_str: String = row.get(7)?;
    let owner_id = owner_id_str
        .parse()
        .map_err(|_| rusqlite::Error::InvalidQuery)?;

    Ok(AnonGroup {
        id,
        name: row.get(1)?,
        owner_id,
        my_role: AnonGroupRole::from_i32(row.get(2)?),
        status: AnonGroupStatus::from_i32(status),
        bootstrap_complete: row.get::<_, i32>(3)? != 0,
        created_at: parse_datetime(&row.get::<_, String>(4)?),
        updated_at: parse_datetime(&row.get::<_, String>(5)?),
    })
}

pub(super) fn row_to_member(row: &rusqlite::Row<'_>) -> rusqlite::Result<AnonGroupMember> {
    let group_id_str: String = row.get(1)?;
    let group_id = group_id_str
        .parse()
        .map_err(|_| rusqlite::Error::InvalidQuery)?;

    let user_id_str: String = row.get(2)?;
    let user_id = user_id_str
        .parse()
        .map_err(|_| rusqlite::Error::InvalidQuery)?;

    let mailbox_pk = vec_to_array_32(row.get(4)?);
    let profile_key = vec_to_array_32(row.get(6)?);

    Ok(AnonGroupMember {
        id: row.get(0)?,
        group_id,
        user_id,
        role: AnonGroupRole::from_i32(row.get(3)?),
        mailbox_pk,
        profile_key,
        display_name: row.get(7)?,
        avatar: row.get(8)?,
        joined_at: parse_datetime(&row.get::<_, String>(5)?),
        device_id: row.get(9)?,
    })
}

pub(super) fn row_to_mailbox(row: &rusqlite::Row<'_>) -> rusqlite::Result<AnonGroupMailbox> {
    let group_id_str: String = row.get(1)?;
    let group_id = group_id_str
        .parse()
        .map_err(|_| rusqlite::Error::InvalidQuery)?;

    let pk_bytes: Vec<u8> = row.get(2)?;
    let sk_bytes: Vec<u8> = row.get(3)?;

    let public_key = require_array_32(&pk_bytes)
        .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid public key".into()))?;
    let private_key = require_array_64(&sk_bytes)
        .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid private key".into()))?;

    Ok(AnonGroupMailbox {
        id: row.get(0)?,
        group_id,
        public_key,
        private_key,
        token_id: row.get(4)?,
        created_at: parse_datetime(&row.get::<_, String>(5)?),
        expires_at: parse_datetime(&row.get::<_, String>(6)?),
    })
}

// SQLite timestamps are deliberately bounded before any structural mutation.
pub(super) fn membership_timestamp(value: u64) -> Result<i64, StorageError> {
    i64::try_from(value)
        .map_err(|_| StorageError::NotFound("Invalid anonymous membership timestamp".into()))
}

pub(super) fn write_ordered_member(
    conn: &rusqlite::Connection,
    member: &AnonGroupMember,
) -> Result<(), StorageError> {
    conn.execute("INSERT INTO anon_group_members(id,group_id,user_id,role,mailbox_pk,joined_at,profile_key,display_name,avatar,device_id,is_active)
        VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,1) ON CONFLICT(group_id,user_id) DO UPDATE SET
        role = excluded.role, mailbox_pk = COALESCE(excluded.mailbox_pk,mailbox_pk), device_id = COALESCE(excluded.device_id,device_id),
        profile_key = COALESCE(excluded.profile_key,profile_key), display_name = COALESCE(excluded.display_name,display_name), avatar = COALESCE(excluded.avatar,avatar), is_active = 1",
        params![member.id,member.group_id.to_string(),member.user_id.to_string(),member.role.to_i32(),
            member.mailbox_pk.as_ref().map(|v|v.as_slice()),member.joined_at.to_rfc3339(),
            member.profile_key.as_ref().map(|v|v.as_slice()),member.display_name,member.avatar,member.device_id])?;
    Ok(())
}

pub(super) fn record_member_change(
    conn: &rusqlite::Connection,
    group: &AnonGroupId,
    user: &UserId,
    timestamp: i64,
) -> Result<(), StorageError> {
    conn.execute("INSERT INTO anon_group_member_order(group_id,user_id,changed_at) VALUES (?1,?2,?3) ON CONFLICT(group_id,user_id) DO UPDATE SET changed_at = MAX(changed_at,excluded.changed_at)",params![group.to_string(),user.to_string(),timestamp])?;
    Ok(())
}
