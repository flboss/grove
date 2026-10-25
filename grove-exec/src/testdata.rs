//! Central test dataset for backend tests (test-only)

use chrono::{DateTime, TimeDelta};
use grove_schema::validated::{FieldId, RelationId, StructId, ValidatedSchema};
use rusqlite::{Connection, params};
use rust_decimal::Decimal;

pub const SCHEMA: &str = r#"
root users: User;
root roles: Role;
root images: Image;
root messages: Message;
struct User {
  id: Int,
  name: String,
  nickname: ?String,
  pos: Tuple<Float, Float>@(x, y),
  home: ?Tuple<Float, Float>@(hx, hy),
  balance: Dec,
  created: Instant,
  elapsed: Duration,
  active: Bool,
  tags: List<String>@tags_ref via user_tags[user_key, tag_value],
  scores: List<(Int, String)>@scores_ref via user_scores[user_key, (s_int, s_str)],
  lucky: List<?Int>@lucky_ref via user_lucky[user_key, lucky_num],
  matrix: List<List<Int> via inner_table[ref, data]>@matrix_ref via outer_table[ref, inner_ref],
  deep_branches: List<(List<List<Int> via deep_inner[dkey, dval]> via deep_mid[dkey, dlink], List<Float> via deep_float[dkey, dval], Bool)>@deep_ref via deep_outer[dkey, (el1, el2, eflag)],
  extra: Tuple<Int, List<String> via extra_tags[ekey, eval]>@(eid, elink),
  opt_extra: ?Tuple<Int, List<String> via opt_tags[okey, oval]>@(oid, olink),
  profile: ?Profile,
  orders: List<Order>,
  roles: &List<Role>,
  manager: &?User,
  subordinates: &List<User>,
}
struct Profile {
  bio: String,
  user: &User,
  notes: List<String>@notes_ref via profile_notes[profile_key, note],
}
struct Order {
  amount: Int,
  user: &User,
}
struct Image {
  title: String,
  messages: &List<Message>,
}
struct Message {
  body: String,
  image: &?Image,
}
struct Role {
  name: String,
  users: &List<User>,
}
rel Profile.user <-> User.profile (profiles.user_id -> users.id);
rel Order.user <<-> User.orders (orders.user_id -> users.id);
rel Image.messages <->> Message.image (messages.image_id -> images.id);
rel Role.users <<->> User.roles via user_roles[role_id -> roles.id, user_id -> users.id];
rel User.manager <<-> User.subordinates (users.manager_id -> users.id);
"#;

pub fn schema() -> ValidatedSchema {
    let (schema, diags) = grove_schema::parse_schema(SCHEMA);
    assert!(diags.is_empty(), "parse diagnostics: {diags:?}");
    let (valid, diags) = grove_schema::validate(schema.expect("fixture parses"));
    assert!(diags.is_empty(), "validation diagnostics: {diags:?}");
    valid.expect("fixture validates")
}

pub fn struct_id(schema: &ValidatedSchema, name: &str) -> StructId {
    StructId::new(schema.structs.iter().position(|s| s.name == name).unwrap())
}

pub fn field_id(schema: &ValidatedSchema, sid: StructId, name: &str) -> FieldId {
    schema.struct_field(sid, name).unwrap().0
}

/// Relation by its endpoints in written order (matters for direction)
pub fn relation_id(schema: &ValidatedSchema, first: FieldId, second: FieldId) -> RelationId {
    RelationId::new(
        schema
            .relations
            .iter()
            .position(|r| r.endpoints() == [first, second])
            .unwrap(),
    )
}

/// Create the dataset tables and rows in an empty connection
pub fn seed(conn: &mut Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE users(id INTEGER, name TEXT, nickname TEXT, x REAL, y REAL,
           hx REAL, hy REAL, balance BLOB, created BLOB, elapsed BLOB, active INTEGER,
           tags_ref INTEGER, scores_ref INTEGER, lucky_ref INTEGER, matrix_ref INTEGER,
           deep_ref INTEGER, eid INTEGER, elink INTEGER, oid INTEGER, olink INTEGER,
           manager_id INTEGER);
         CREATE TABLE user_tags(user_key INTEGER, tag_value TEXT);
         CREATE TABLE user_scores(user_key INTEGER, s_int INTEGER, s_str TEXT);
         CREATE TABLE user_lucky(user_key INTEGER, lucky_num INTEGER);
         CREATE TABLE outer_table(ref INTEGER, inner_ref INTEGER);
         CREATE TABLE inner_table(ref INTEGER, data INTEGER);
         INSERT INTO user_tags(user_key, tag_value) VALUES
           (20, 'b2'), (10, 'a1'), (10, 'a2'), (20, 'b1');
         INSERT INTO user_scores(user_key, s_int, s_str) VALUES
           (100, 1, 'one'), (100, 2, 'two'), (200, 3, 'three');
         INSERT INTO user_lucky(user_key, lucky_num) VALUES
           (1000, 5), (1000, NULL);
         INSERT INTO outer_table(ref, inner_ref) VALUES (500, 50), (500, 60);
         INSERT INTO inner_table(ref, data) VALUES (50, 1), (50, 2);
         CREATE TABLE deep_outer(dkey INTEGER, el1 INTEGER, el2 INTEGER, eflag INTEGER);
         CREATE TABLE deep_mid(dkey INTEGER, dlink INTEGER);
         CREATE TABLE deep_inner(dkey INTEGER, dval INTEGER);
         CREATE TABLE deep_float(dkey INTEGER, dval REAL);
         INSERT INTO deep_outer(dkey, el1, el2, eflag) VALUES (600, 61, 62, 1);
         INSERT INTO deep_mid(dkey, dlink) VALUES (61, 611), (61, 612);
         INSERT INTO deep_inner(dkey, dval) VALUES (611, 1), (611, 2);
         INSERT INTO deep_float(dkey, dval) VALUES (62, 2.5);
         CREATE TABLE extra_tags(ekey INTEGER, eval TEXT);
         CREATE TABLE opt_tags(okey INTEGER, oval TEXT);
         INSERT INTO extra_tags(ekey, eval) VALUES (910, 'x'), (910, 'y'), (950, 'z');
         INSERT INTO opt_tags(okey, oval) VALUES (960, 'p');
         CREATE TABLE profiles(bio TEXT, user_id INTEGER, notes_ref INTEGER);
         CREATE TABLE profile_notes(profile_key INTEGER, note TEXT);
         CREATE TABLE orders(amount INTEGER, user_id INTEGER);
         CREATE TABLE images(id INTEGER, title TEXT);
         CREATE TABLE messages(body TEXT, image_id INTEGER);
         CREATE TABLE roles(id INTEGER, name TEXT);
         CREATE TABLE user_roles(role_id INTEGER, user_id INTEGER);
         INSERT INTO profiles(bio, user_id, notes_ref) VALUES
           ('AdaBio', 1, 501), ('CydBio', 3, NULL);
         INSERT INTO profile_notes(profile_key, note) VALUES (501, 'a-note');
         INSERT INTO orders(amount, user_id) VALUES (100, 1), (200, 1), (300, NULL), (400, 2);
         INSERT INTO images(id, title) VALUES (1, 'Sunset'), (2, 'Portrait');
         INSERT INTO messages(body, image_id) VALUES
           ('nice', 1), ('wow', 1), ('meh', 2), ('lost', NULL);
         INSERT INTO roles(id, name) VALUES (1, 'admin'), (2, 'editor');
         INSERT INTO user_roles(role_id, user_id) VALUES (1, 1), (2, 1), (2, 3), (2, 1);",
    )?;
    let insert = "INSERT INTO users(id, name, nickname, x, y, hx, hy, balance, created, \
                  elapsed, active, tags_ref, scores_ref, lucky_ref, matrix_ref, deep_ref, \
                  eid, elink, oid, olink, manager_id) \
                  VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";
    struct SeedUser {
        id: i64,
        name: &'static str,
        nick: Option<&'static str>,
        x: f64,
        y: f64,
        hx: Option<f64>,
        hy: Option<f64>,
        balance: &'static str,
        secs: i64,
        nanos: u32,
        elapsed: TimeDelta,
        active: i64,
        tags: Option<i64>,
        scores: Option<i64>,
        lucky: Option<i64>,
        matrix: Option<i64>,
        deep_branches: Option<i64>,
        eid: Option<i64>,
        elink: Option<i64>,
        oid: Option<i64>,
        olink: Option<i64>,
        manager: Option<i64>,
    }
    let users = [
        SeedUser {
            id: 1,
            name: "Ada",
            nick: Some("A"),
            x: 1.5,
            y: -2.5,
            hx: Some(3.0),
            hy: Some(4.0),
            balance: "30.00",
            secs: 1_700_000_000,
            nanos: 0,
            elapsed: TimeDelta::seconds(90),
            active: 1,
            tags: Some(10),
            scores: Some(100),
            lucky: Some(1000),
            matrix: Some(500),
            deep_branches: Some(600),
            eid: Some(1),
            elink: Some(910),
            oid: Some(5),
            olink: Some(960),
            manager: Some(2),
        },
        SeedUser {
            id: 2,
            name: "Bob",
            nick: None,
            x: 0.0,
            y: 0.0,
            hx: None,
            hy: None,
            balance: "0",
            secs: 0,
            nanos: 0,
            elapsed: TimeDelta::zero(),
            active: 1,
            tags: Some(20),
            scores: None,
            lucky: None,
            matrix: None,
            deep_branches: None,
            eid: Some(3),
            elink: None,
            oid: None,
            olink: None,
            manager: None,
        },
        SeedUser {
            id: 3,
            name: "Cyd",
            nick: Some("C"),
            x: -1.25,
            y: 7.0,
            hx: None,
            hy: None,
            balance: "-5.25",
            secs: -1,
            nanos: 500_000_000,
            elapsed: TimeDelta::milliseconds(-1500),
            active: 0,
            tags: None,
            scores: None,
            lucky: None,
            matrix: None,
            deep_branches: None,
            eid: Some(4),
            elink: None,
            oid: None,
            olink: None,
            manager: Some(2),
        },
        SeedUser {
            id: 4,
            name: "Dan",
            nick: None,
            x: 2.0,
            y: 2.0,
            hx: Some(0.0),
            hy: Some(1.0),
            balance: "1000000",
            secs: 4_102_444_800,
            nanos: 0,
            elapsed: TimeDelta::milliseconds(1500),
            active: 1,
            tags: Some(30),
            scores: Some(200),
            lucky: None,
            matrix: Some(700),
            deep_branches: Some(650),
            eid: Some(2),
            elink: Some(950),
            oid: Some(7),
            olink: None,
            manager: Some(1),
        },
        SeedUser {
            id: 5,
            name: "Eli",
            nick: Some("E"),
            x: 9.5,
            y: -0.5,
            hx: None,
            hy: None,
            balance: "0.1",
            secs: 1_700_000_000,
            nanos: 123_456_789,
            elapsed: TimeDelta::nanoseconds(123_456_789),
            active: 0,
            tags: Some(10),
            scores: None,
            lucky: Some(2000),
            matrix: None,
            deep_branches: None,
            eid: Some(5),
            elink: None,
            oid: None,
            olink: None,
            manager: None,
        },
    ];
    for u in users {
        let created = DateTime::from_timestamp(u.secs, u.nanos).unwrap();
        let balance = crate::codec::encode_dec(u.balance.parse::<Decimal>().unwrap());
        conn.execute(
            insert,
            params![
                u.id,
                u.name,
                u.nick,
                u.x,
                u.y,
                u.hx,
                u.hy,
                &balance[..],
                &crate::codec::encode_instant(created)[..],
                &crate::codec::encode_duration(u.elapsed)[..],
                u.active,
                u.tags,
                u.scores,
                u.lucky,
                u.matrix,
                u.deep_branches,
                u.eid,
                u.elink,
                u.oid,
                u.olink,
                u.manager,
            ],
        )?;
    }
    Ok(())
}
