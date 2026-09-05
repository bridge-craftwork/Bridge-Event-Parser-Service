//! BWS + PBN → NormalizedGame adapter.
//!
//! Reads a BWS file (Berkshire Bridge Score) for results and player
//! identities, plus an optional PBN file for hand records / DD / par,
//! and emits a single-tournament, single-event, single-session
//! NormalizedGame document.

use crate::error::{AnalysisError, Result};
use crate::schema::{
    Board as SchemaBoard, DdStrains as SchemaDdStrains, Deal as SchemaDeal,
    DoubleDummy as SchemaDoubleDummy, Event, Hand as SchemaHand, NormalizedGame,
    Pair as SchemaPair, Par as SchemaPar, Player as SchemaPlayer, Result as SchemaResult, Session,
    Tournament,
};
use crate::types::{BoardData, ContractResult, ParsedContract, SeatPlayers};
use bridge_parsers::bws::{read_bws, BwsData};
use bridge_parsers::pbn::read_pbn;
use bridge_parsers::{Board as PbnBoard, Contract, Deal, Direction, Strain, Suit, Vulnerability};
use bridge_types::DdTable;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Read BWS + (optional) PBN files and produce a single-session NormalizedGame.
///
/// `overrides` maps ACBL number → real name. Placeholder names like
/// "Player N-3" (synthesized when the BWS row has no `Name`) are replaced
/// by the override at emit time, so the JSON downstream has clean names.
pub fn load_normalized(
    bws_path: &Path,
    pbn_path: Option<&Path>,
    overrides: Option<&HashMap<String, String>>,
) -> Result<NormalizedGame> {
    let bws_data = read_bws(bws_path)?;

    let pbn_boards = if let Some(p) = pbn_path {
        let content = std::fs::read_to_string(p)?;
        read_pbn(&content).map_err(|e| AnalysisError::Pbn(e.to_string()))?
    } else {
        Vec::new()
    };

    let (event_name, event_date) = bws_data
        .sessions
        .first()
        .map(|s| {
            (
                s.name
                    .as_ref()
                    .map(|n| n.trim().to_string())
                    .filter(|n| !n.is_empty()),
                s.date
                    .as_ref()
                    .map(|d| d.trim().to_string())
                    .filter(|d| !d.is_empty()),
            )
        })
        .unwrap_or((None, None));

    let normalized_date = event_date.as_deref().map(normalize_date);

    // Build pair-number → (player_at_seat1, player_at_seat2) lookup. For NS
    // pairs seat1=N, seat2=S; for EW seat1=W, seat2=E (display ordering).
    let pair_lookup = build_pair_lookup(&bws_data);

    // Index PBN boards by number for fast deal/dd/par lookup.
    let mut pbn_by_number: HashMap<u32, &PbnBoard> = HashMap::new();
    for board in &pbn_boards {
        if let Some(num) = board.number {
            pbn_by_number.insert(num, board);
        }
    }

    // Walk results, building schema Boards + Results in one pass.
    let mut boards_by_num: HashMap<u32, SchemaBoard> = HashMap::new();
    // Dedup on (board, section, table, round) — same key the legacy loader used.
    let mut seen: HashSet<(i32, i32, i32, i32)> = HashSet::new();

    for received in &bws_data.received_data {
        if received.contract.is_empty() {
            continue;
        }
        let board_number = received.board as u32;
        let is_passout = received.contract.eq_ignore_ascii_case("PASS");

        // Resolve players (with override application)
        let ((mut n_name, n_acbl), (mut s_name, s_acbl)) = pair_lookup
            .get(&(received.section, received.pair_ns, true))
            .cloned()
            .unwrap_or_else(|| {
                resolve_pair_from_table(&bws_data, received.section, received.pair_ns, "NS")
            });
        let ((mut w_name, w_acbl), (mut e_name, e_acbl)) = pair_lookup
            .get(&(received.section, received.pair_ew, false))
            .cloned()
            .unwrap_or_else(|| {
                resolve_pair_from_table(&bws_data, received.section, received.pair_ew, "EW")
            });

        if let Some(o) = overrides {
            apply_override(&mut n_name, &n_acbl, o);
            apply_override(&mut s_name, &s_acbl, o);
            apply_override(&mut e_name, &e_acbl, o);
            apply_override(&mut w_name, &w_acbl, o);
        }

        // Make sure the schema Board exists for this number, populating from
        // PBN if available (deal/DD/par), otherwise minimal info from BWS.
        boards_by_num
            .entry(board_number)
            .or_insert_with(|| build_schema_board(board_number, pbn_by_number.get(&board_number)));

        // Skip duplicates after we've ensured the board entry exists, so the
        // board is still in the document even if the only result row is a dup.
        let dup_key = (
            received.board,
            received.section,
            received.table,
            received.round,
        );
        if !is_passout && !seen.insert(dup_key) {
            continue;
        }

        let section_str = section_letter(received.section);

        let ns_pair = SchemaPair {
            number: Some(received.pair_ns),
            section: Some(section_str.clone()),
            players: vec![
                schema_player(&n_name, n_acbl),
                schema_player(&s_name, s_acbl),
            ],
            strat: None,
            strat_ranks: vec![],
        };
        let ew_pair = SchemaPair {
            number: Some(received.pair_ew),
            section: Some(section_str),
            players: vec![
                schema_player(&w_name, w_acbl),
                schema_player(&e_name, e_acbl),
            ],
            strat: None,
            strat_ranks: vec![],
        };

        let result = if is_passout {
            SchemaResult {
                contract: Some("PASS".to_string()),
                declarer: None,
                tricks: None,
                score: Some(0),
                matchpoints: None,
                percentage: None,
                imps: None,
                ns_pair,
                ew_pair,
                auction: None,
                play: None,
                handviewer_url: None,
            }
        } else {
            let declarer_dir = parse_declarer_direction(&received.ns_ew);
            let contract = ParsedContract::parse(&received.contract);
            let tricks_relative = Contract::parse_result(&received.result);
            let tricks_absolute = match (contract.as_ref(), tricks_relative) {
                (Some(c), Some(rel)) => {
                    let n = c.level as i32 + 6 + rel;
                    if (0..=13).contains(&n) {
                        Some(n as u8)
                    } else {
                        None
                    }
                }
                _ => None,
            };

            let pbn_ref = pbn_by_number.get(&board_number);
            let vulnerability = pbn_ref.map(|b| b.vulnerable).unwrap_or(Vulnerability::None);
            let ns_score = compute_ns_score(
                contract.as_ref(),
                tricks_relative,
                declarer_dir,
                vulnerability,
            );

            // Build the BBO hand-viewer URL once at adapter time so the
            // schema's per-result `handviewer_url` is populated. Otherwise
            // engine=js mode (which reads the schema directly) wouldn't
            // have an iframe URL to load. We construct a minimal BoardData
            // and reuse the existing BoardData::bbo_handviewer_url impl
            // (it only reads deal / dealer / vul / number, not DD or par).
            let handviewer_url = pbn_ref
                .filter(|b| b.deal.has_cards())
                .and_then(|pbn_board| {
                    let board_data = BoardData {
                        number: board_number,
                        dealer: pbn_board.dealer.unwrap_or(Direction::North),
                        vulnerability,
                        deal: Some(pbn_board.deal.clone()),
                    };
                    // Apply the same display_name() transform the server's
                    // /api/board path uses (lowercase + first-letter-cap)
                    // so the URL matches what the engine=server response
                    // produces. Otherwise BWS files with mixed-case names
                    // ("LaFrancesca") encode differently in LIN.
                    let seat_players = SeatPlayers {
                        north: title_case(&n_name),
                        south: title_case(&s_name),
                        east: title_case(&e_name),
                        west: title_case(&w_name),
                    };
                    let contract_result = contract.as_ref().map(|c| ContractResult {
                        contract: c.clone(),
                        declarer: declarer_dir,
                    });
                    board_data.bbo_handviewer_url(Some(&seat_players), contract_result.as_ref())
                });

            SchemaResult {
                contract: contract.as_ref().map(|c| c.display()),
                declarer: Some(direction_str(declarer_dir).to_string()),
                tricks: tricks_absolute,
                score: Some(ns_score),
                matchpoints: None,
                percentage: None,
                imps: None,
                ns_pair,
                ew_pair,
                auction: None,
                play: None,
                handviewer_url,
            }
        };

        if let Some(b) = boards_by_num.get_mut(&board_number) {
            b.results.push(result);
        }
    }

    // Also surface PBN boards that had no BWS results (rare but possible).
    for board in &pbn_boards {
        if let Some(num) = board.number {
            boards_by_num
                .entry(num)
                .or_insert_with(|| build_schema_board(num, Some(&board)));
        }
    }

    let mut boards: Vec<SchemaBoard> = boards_by_num.into_values().collect();
    boards.sort_by_key(|b| b.number);

    let session = Session {
        session_number: 1,
        time: None,
        user_pair: None,
        pairs: None,
        boards,
        partial: false,
        warnings: Vec::new(),
    };

    let event = Event {
        event_id: None,
        event_type: None,
        name: event_name.clone(),
        date: normalized_date,
        scoring: Some("matchpoints".to_string()),
        sessions: vec![session],
    };

    let tournament = Tournament {
        sanction: None,
        schedule_url: None,
        name: event_name,
        events: vec![event],
    };

    Ok(NormalizedGame {
        schema_version: "1.0".to_string(),
        source: "pbn-bws".to_string(),
        fetched_at: chrono::Utc::now().to_rfc3339(),
        source_url: None,
        tournaments: vec![tournament],
    })
}

/// Build a schema Board from an optional PBN reference. Falls back to a
/// minimal entry (number + dealer N + None vul) when PBN data is absent.
fn build_schema_board(number: u32, pbn: Option<&&PbnBoard>) -> SchemaBoard {
    let pbn = pbn.copied();
    let dealer = pbn.and_then(|b| b.dealer).unwrap_or(Direction::North);
    let vul = pbn.map(|b| b.vulnerable).unwrap_or(Vulnerability::None);
    let deal = pbn.and_then(|b| {
        if b.deal.has_cards() {
            Some(deal_to_schema(&b.deal))
        } else {
            None
        }
    });
    let double_dummy = pbn
        .and_then(|b| b.double_dummy_tricks.as_ref())
        .map(dd_table_to_schema);
    let par = pbn
        .map(|b| par_strings_to_schema(b.par_contract.as_deref(), b.optimum_score.as_deref(), vul))
        .unwrap_or_default();

    SchemaBoard {
        number,
        section: None,
        dealer: direction_str(dealer).to_string(),
        vulnerability: vulnerability_str(vul).to_string(),
        deal,
        double_dummy,
        par,
        results: Vec::new(),
        user_result_index: None,
    }
}

fn deal_to_schema(deal: &Deal) -> SchemaDeal {
    SchemaDeal {
        north: hand_to_schema(deal.hand(Direction::North)),
        east: hand_to_schema(deal.hand(Direction::East)),
        south: hand_to_schema(deal.hand(Direction::South)),
        west: hand_to_schema(deal.hand(Direction::West)),
    }
}

fn hand_to_schema(hand: &bridge_parsers::Hand) -> SchemaHand {
    SchemaHand {
        spades: cards_in_suit(hand, Suit::Spades),
        hearts: cards_in_suit(hand, Suit::Hearts),
        diamonds: cards_in_suit(hand, Suit::Diamonds),
        clubs: cards_in_suit(hand, Suit::Clubs),
    }
}

fn cards_in_suit(hand: &bridge_parsers::Hand, suit: Suit) -> Vec<String> {
    let mut cards = hand.cards_in_suit(suit);
    cards.sort_by_key(|c| std::cmp::Reverse(c.rank));
    cards
        .into_iter()
        .map(|c| {
            // bridge_parsers::Rank uses 'T' for ten; schema uses "10".
            let ch = c.rank.to_char();
            if ch == 'T' {
                "10".to_string()
            } else {
                ch.to_string()
            }
        })
        .collect()
}

/// Project a decoded double-dummy table onto the schema's per-declarer object.
///
/// `DdTable` is addressed by seat and strain, so this cannot transpose: the
/// schema's own field names decide which cell each value comes from. Every
/// cell of a decoded table is known, so all twenty are `Some`; the schema
/// keeps them optional only because it also has to describe partial data from
/// other sources.
fn dd_table_to_schema(table: &DdTable) -> SchemaDoubleDummy {
    let strains_for = |declarer: Direction| SchemaDdStrains {
        no_trump: Some(table.tricks(declarer, Strain::NoTrump)),
        spades: Some(table.tricks(declarer, Strain::Spades)),
        hearts: Some(table.tricks(declarer, Strain::Hearts)),
        diamonds: Some(table.tricks(declarer, Strain::Diamonds)),
        clubs: Some(table.tricks(declarer, Strain::Clubs)),
    };
    SchemaDoubleDummy {
        north: Some(strains_for(Direction::North)),
        south: Some(strains_for(Direction::South)),
        east: Some(strains_for(Direction::East)),
        west: Some(strains_for(Direction::West)),
    }
}

/// Parse PBN par strings into the schema's Vec<Par>. Tied par "N 4H=; S 4S="
/// becomes two entries with the same score.
fn par_strings_to_schema(
    par_str: Option<&str>,
    score_str: Option<&str>,
    _vulnerability: Vulnerability,
) -> Vec<SchemaPar> {
    let par_str = match par_str {
        Some(s) if !s.trim().is_empty() => s,
        _ => return Vec::new(),
    };

    let signed_score: i32 = score_str
        .map(|s| {
            let mut tokens = s.split_whitespace();
            let side = tokens.next().unwrap_or("");
            let magnitude: i32 = tokens.next().and_then(|t| t.parse().ok()).unwrap_or(0);
            match side {
                "NS" | "N" | "S" => magnitude,
                "EW" | "E" | "W" => -magnitude,
                _ => 0,
            }
        })
        .unwrap_or(0);

    let mut out = Vec::new();
    for part in par_str.split([';', ',']) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let mut tokens = part.split_whitespace();
        let side = match tokens.next() {
            Some(s) => s,
            None => continue,
        };
        let body = match tokens.next() {
            Some(s) => s,
            None => continue,
        };

        // Split off result suffix (=, +N, -N) — we keep just the contract part.
        let contract_part = body.split(['=', '+', '-']).next().unwrap_or(body);
        let contract = match ParsedContract::parse(contract_part) {
            Some(c) => c,
            None => continue,
        };

        let declarer = match side {
            "N" => Direction::North,
            "E" => Direction::East,
            "S" => Direction::South,
            "W" => Direction::West,
            "NS" => Direction::North,
            "EW" => Direction::East,
            _ => continue,
        };

        out.push(SchemaPar {
            score: signed_score,
            contract: contract.display(),
            declarer: direction_str(declarer).to_string(),
        });
    }
    out
}

/// Compute NS-perspective score from contract + tricks_relative + vul.
fn compute_ns_score(
    contract: Option<&ParsedContract>,
    tricks_relative: Option<i32>,
    declarer: Direction,
    vulnerability: Vulnerability,
) -> i32 {
    let contract = match contract {
        Some(c) => c,
        None => return 0,
    };
    let rel = match tricks_relative {
        Some(t) => t,
        None => return 0,
    };
    let bp = Contract::new(
        contract.level,
        contract.strain,
        contract.doubled,
        direction_char(declarer),
    );
    let vulnerable = vulnerability.is_vulnerable(declarer);
    let raw = bp.score(rel, vulnerable);
    match declarer {
        Direction::North | Direction::South => raw,
        Direction::East | Direction::West => -raw,
    }
}

fn schema_player(name: &str, acbl: Option<String>) -> SchemaPlayer {
    SchemaPlayer {
        name: name.to_string(),
        acbl_id: acbl,
        external_ids: HashMap::new(),
        masterpoints_earned: vec![],
    }
}

fn apply_override(name: &mut String, acbl: &Option<String>, overrides: &HashMap<String, String>) {
    if name.starts_with("Player ") {
        if let Some(num) = acbl {
            if let Some(real) = overrides.get(num) {
                *name = real.clone();
            }
        }
    }
}

/// Map BWS section integer to a single-letter section name, mirroring the
/// builder's section_key inverse: 1→"A", 2→"B", ..., otherwise "A".
fn section_letter(section: i32) -> String {
    if (1..=26).contains(&section) {
        let ch = (b'A' + (section - 1) as u8) as char;
        ch.to_string()
    } else {
        "A".to_string()
    }
}

/// "03/30/26 00:00:00" → "2026-03-30". Falls back to the original string.
fn normalize_date(raw: &str) -> String {
    let date_part = raw.split_whitespace().next().unwrap_or(raw);
    let parts: Vec<&str> = date_part.split('/').collect();
    if parts.len() == 3 {
        let year = if parts[2].len() == 2 {
            format!("20{}", parts[2])
        } else {
            parts[2].to_string()
        };
        format!("{}-{:0>2}-{:0>2}", year, parts[0], parts[1])
    } else {
        date_part.to_string()
    }
}

/// Mirror of identity::PlayerId::display_name(): lowercase the canonical
/// form, then capitalize the first letter of each whitespace-separated
/// word. Applied to player names when building the LIN URL so the
/// adapter-emitted URL matches what /api/board produces.
fn title_case(name: &str) -> String {
    name.trim()
        .to_lowercase()
        .split_whitespace()
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                None => String::new(),
                Some(c) => c.to_uppercase().chain(chars).collect(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_declarer_direction(ns_ew: &str) -> Direction {
    match ns_ew.trim().to_uppercase().as_str() {
        "N" => Direction::North,
        "S" => Direction::South,
        "E" => Direction::East,
        "W" => Direction::West,
        _ => Direction::North,
    }
}

fn direction_str(d: Direction) -> &'static str {
    match d {
        Direction::North => "N",
        Direction::East => "E",
        Direction::South => "S",
        Direction::West => "W",
    }
}

fn direction_char(d: Direction) -> char {
    match d {
        Direction::North => 'N',
        Direction::East => 'E',
        Direction::South => 'S',
        Direction::West => 'W',
    }
}

fn vulnerability_str(v: Vulnerability) -> &'static str {
    match v {
        Vulnerability::None => "None",
        Vulnerability::NorthSouth => "NS",
        Vulnerability::EastWest => "EW",
        Vulnerability::Both => "Both",
    }
}

// ==================== Pair-number lookup (BWS-specific) ====================

type PlayerEntry = (String, Option<String>);

/// Build a (section, pair_number, is_ns) → ((name1,acbl1),(name2,acbl2)) lookup.
/// Uses RoundData round 1 to map pair numbers to physical seats, then
/// PlayerNumbers for names + ACBL ids.
fn build_pair_lookup(bws_data: &BwsData) -> HashMap<(i32, i32, bool), (PlayerEntry, PlayerEntry)> {
    let mut player_at: HashMap<(i32, i32, &'static str), PlayerEntry> = HashMap::new();
    for pn in &bws_data.player_numbers {
        let dir: &'static str = match pn.direction.as_str() {
            "N" => "N",
            "S" => "S",
            "E" => "E",
            "W" => "W",
            _ => continue,
        };
        let name = pn
            .name
            .as_ref()
            .filter(|n| !n.is_empty())
            .cloned()
            .unwrap_or_else(|| format!("Player {}-{}", dir, pn.table));
        let acbl = if pn.number.is_empty() {
            None
        } else {
            Some(pn.number.clone())
        };
        player_at.insert((pn.section, pn.table, dir), (name, acbl));
    }

    let mut pair_lookup: HashMap<(i32, i32, bool), (PlayerEntry, PlayerEntry)> = HashMap::new();
    if bws_data.round_data.is_empty() {
        return pair_lookup;
    }

    for rd in &bws_data.round_data {
        if rd.round != 1 {
            continue;
        }
        if rd.ns_pair > 0 {
            let p1 = player_at.get(&(rd.section, rd.table, "N"));
            let p2 = player_at.get(&(rd.section, rd.table, "S"));
            if let (Some(p1), Some(p2)) = (p1, p2) {
                let entries = (p1.clone(), p2.clone());
                pair_lookup.insert((rd.section, rd.ns_pair, true), entries.clone());
                if rd.ns_pair != rd.ew_pair {
                    pair_lookup.insert((rd.section, rd.ns_pair, false), entries);
                }
            }
        }
        if rd.ew_pair > 0 {
            let p1 = player_at.get(&(rd.section, rd.table, "W"));
            let p2 = player_at.get(&(rd.section, rd.table, "E"));
            if let (Some(p1), Some(p2)) = (p1, p2) {
                let entries = (p1.clone(), p2.clone());
                pair_lookup.insert((rd.section, rd.ew_pair, false), entries.clone());
                if rd.ns_pair != rd.ew_pair {
                    pair_lookup.insert((rd.section, rd.ew_pair, true), entries);
                }
            }
        }
    }
    pair_lookup
}

/// Mitchell fallback for when the round-1 lookup misses. Looks up by table.
fn resolve_pair_from_table(
    bws_data: &BwsData,
    section: i32,
    table: i32,
    dir: &str,
) -> (PlayerEntry, PlayerEntry) {
    let (d1, d2) = match dir {
        "NS" => ("N", "S"),
        _ => ("W", "E"),
    };
    let lookup_seat = |d: &str| -> PlayerEntry {
        let pn = bws_data
            .player_numbers
            .iter()
            .find(|p| p.section == section && p.table == table && p.direction == d);
        match pn {
            Some(pn) => {
                let name = pn
                    .name
                    .as_ref()
                    .filter(|n| !n.is_empty())
                    .cloned()
                    .unwrap_or_else(|| format!("Player {}-{}", d, table));
                let acbl = if pn.number.is_empty() {
                    None
                } else {
                    Some(pn.number.clone())
                };
                (name, acbl)
            }
            None => (format!("Player {}-{}", d, table), None),
        }
    };
    (lookup_seat(d1), lookup_seat(d2))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read one board from a PBN fragment.
    fn read_one(pbn: &str) -> PbnBoard {
        let boards = read_pbn(pbn).expect("fragment parses");
        boards.into_iter().next().expect("one board")
    }

    /// Serialise the `double_dummy` member of the schema board built from `pbn`.
    fn dd_json(pbn: &str) -> String {
        let board = read_one(pbn);
        let schema = build_schema_board(1, Some(&&board));
        serde_json::to_string(&schema.double_dummy).expect("serialises")
    }

    fn fragment(dd_value: &str) -> String {
        format!(
            "[Board \"1\"]\n[Dealer \"N\"]\n[Vulnerable \"None\"]\n[DoubleDummyTricks \"{dd_value}\"]\n"
        )
    }

    /// Pins the seat/strain mapping of `DoubleDummyTricks` to the emitted JSON.
    ///
    /// The tag is seat-major in N, S, E, W and strain-major in NT, S, H, D, C.
    /// Both expectations here are byte-for-byte what this adapter emitted
    /// before `double_dummy_tricks` became a decoded `DdTable`, so a
    /// transposition of either axis fails the test rather than quietly
    /// changing the JSON.
    #[test]
    fn double_dummy_tag_maps_to_seats_and_strains() {
        // Symmetric across partners, so it only pins the strain axis.
        assert_eq!(
            dd_json(&fragment("56865568656757867578")),
            r#"{"N":{"C":5,"D":6,"H":8,"S":6,"NT":5},"E":{"C":8,"D":7,"H":5,"S":7,"NT":6},"S":{"C":5,"D":6,"H":8,"S":6,"NT":5},"W":{"C":8,"D":7,"H":5,"S":7,"NT":6}}"#
        );
        // All twenty cells distinct enough that any transposition of seats or
        // strains moves at least one value.
        assert_eq!(
            dd_json(&fragment("da0b19c8375624ac0d13")),
            r#"{"N":{"C":1,"D":11,"H":0,"S":10,"NT":13},"E":{"C":10,"D":4,"H":2,"S":6,"NT":5},"S":{"C":7,"D":3,"H":8,"S":12,"NT":9},"W":{"C":3,"D":1,"H":13,"S":0,"NT":12}}"#
        );
    }

    /// `a`-`d` and `A`-`D` both mean ten to thirteen.
    #[test]
    fn double_dummy_digits_are_case_insensitive() {
        assert_eq!(
            dd_json(&fragment("abcd0000000000000000")),
            dd_json(&fragment("ABCD0000000000000000"))
        );
    }

    /// A malformed tag value drops the whole table, not just the bad cell.
    ///
    /// The previous private decoder was lenient per character: a bad digit
    /// left one `null` cell in an otherwise populated table. The shared
    /// decoder in bridge-encodings rejects the value outright, so
    /// `double_dummy` is absent. Absent is the honest answer — a table with
    /// one hole cannot be told apart from one the producer meant to leave
    /// partial — and this service already emits `null` for boards with no
    /// analysis at all.
    #[test]
    fn malformed_double_dummy_yields_no_table() {
        // 19 characters.
        assert_eq!(dd_json(&fragment("5686556865675786757")), "null");
        // 20 characters, one of them not a trick count.
        assert_eq!(dd_json(&fragment("5686556865675786757z")), "null");
        // 'e' would be 14 tricks.
        assert_eq!(dd_json(&fragment("e6865568656757867578")), "null");
        // Absent tag.
        assert_eq!(dd_json("[Board \"1\"]\n[Dealer \"N\"]\n"), "null");
    }

    /// `dd_table_to_schema` reads by seat and strain, with no PBN codec in the
    /// way: one cell set, one cell non-zero.
    #[test]
    fn table_cells_land_in_the_named_fields() {
        let mut table = DdTable::new();
        table.set(Direction::West, Strain::Hearts, 11);
        let schema = dd_table_to_schema(&table);
        assert_eq!(
            serde_json::to_string(&schema).expect("serialises"),
            r#"{"N":{"C":0,"D":0,"H":0,"S":0,"NT":0},"E":{"C":0,"D":0,"H":0,"S":0,"NT":0},"S":{"C":0,"D":0,"H":0,"S":0,"NT":0},"W":{"C":0,"D":0,"H":11,"S":0,"NT":0}}"#
        );
    }
}
