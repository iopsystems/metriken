//! `form` against a brute-force reference of the same rules, on seeded
//! random cases: every pair of uuid groups related directly, bridges found
//! by trying every pair, the union taken over every pair. The cases mix
//! uuids shared and not, arms (an armed archive in two of five cases),
//! hosts, sources without rows, one epoch or a `producer_epochs` list in any
//! order, rows on a grid, and row checks that cannot tell (an unreadable
//! instant, or answers drawn at random).
#![allow(clippy::needless_range_loop)]

use super::*;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
}

#[derive(Clone, Debug)]
struct Case {
    sources: Vec<SourceInfo>,
    rows: Vec<Vec<u64>>,
    unreadable: Vec<Vec<u64>>,
    random_rows: bool,
    seed: u64,
}

fn has_row_model(c: &Case, s: usize, lo: u64, hi: u64) -> Option<bool> {
    if c.random_rows {
        let mut h = Rng(c.seed
            ^ (s as u64).wrapping_mul(7919)
            ^ lo.wrapping_mul(31)
            ^ hi.wrapping_mul(1_000_003));
        return match h.below(3) {
            0 => Some(true),
            1 => Some(false),
            _ => None,
        };
    }
    if lo > hi {
        return Some(false);
    }
    if c.rows[s].iter().any(|&t| lo <= t && t <= hi) {
        return Some(true);
    }
    if c.unreadable[s].iter().any(|&t| lo <= t && t <= hi) {
        return None;
    }
    Some(false)
}

fn gen(seed: u64, n: usize, domain: u64) -> Case {
    let mut r = Rng(seed);
    let random_rows = r.chance(25);
    let armed_archive = r.chance(40);
    let uuid_pool = (n as u64 / 2).max(1);
    let mut sources = Vec::new();
    let mut rows = Vec::new();
    let mut unreadable = Vec::new();
    for i in 0..n {
        let uuid = if r.chance(60) {
            Some(format!("u{}", r.below(uuid_pool)))
        } else {
            None
        };
        let mut labels = BTreeMap::new();
        if armed_archive && r.chance(70) {
            labels.insert(
                "arm".to_string(),
                ["a", "b"][r.below(2) as usize].to_string(),
            );
        }
        if r.chance(30) {
            labels.insert("host".to_string(), format!("h{}", r.below(2)));
        }
        // Rows at integer times * 10.
        let (span, rs, un) = if r.chance(8) {
            (None, vec![], vec![])
        } else {
            let a = r.below(domain);
            let b = a + r.below(domain / 2 + 1);
            let mut rs = vec![a * 10, b * 10];
            for t in a + 1..b {
                if r.chance(50) {
                    rs.push(t * 10);
                }
            }
            rs.sort_unstable();
            rs.dedup();
            let mut un = vec![];
            if r.chance(25) {
                for t in a..=b {
                    if r.chance(15) {
                        un.push(t * 10 + 5);
                    }
                }
            }
            (Some((a * 10, b * 10)), rs, un)
        };
        let mut metadata = BTreeMap::new();
        let ne = r.below(4);
        if ne == 1 {
            metadata.insert(PRODUCER_EPOCH.to_string(), format!("e{}", r.below(3)));
        } else if ne >= 2 {
            let mut list = Vec::new();
            for _ in 0..ne {
                list.push(format!(
                    r#"{{"epoch":"e{}","from_ts":{}}}"#,
                    r.below(3),
                    r.below(domain * 2) * 5
                ));
            }
            metadata.insert(PRODUCER_EPOCHS.to_string(), format!("[{}]", list.join(",")));
        }
        sources.push(SourceInfo::new(labels, i, uuid, span, &metadata));
        rows.push(rs);
        unreadable.push(un);
    }
    Case {
        sources,
        rows,
        unreadable,
        random_rows,
        seed,
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Rel {
    Yes,
    No,
    Unk,
}

/// The first epoch and intersection a pair was checked in without a shared
/// row.
type Miss = Option<(String, (u64, u64))>;

struct Brute {
    uuid_groups: Vec<Vec<usize>>,
    groups: Vec<Vec<usize>>,
    bridges: Vec<usize>,
    notices: Vec<String>,
    rel: Vec<Vec<(Rel, bool)>>, // (relation, shares epoch)
    bridges_over: Vec<Vec<bool>>,
}

fn brute(c: &Case) -> Brute {
    let s = &c.sources;
    let mut ug: Vec<Vec<usize>> = Vec::new();
    let mut uu: Vec<Option<String>> = Vec::new();
    for (p, src) in s.iter().enumerate() {
        match src
            .uuid
            .as_ref()
            .and_then(|u| uu.iter().position(|x| x.as_ref() == Some(u)))
        {
            Some(i) => ug[i].push(p),
            None => {
                ug.push(vec![p]);
                uu.push(src.uuid.clone());
            }
        }
    }
    let n = ug.len();
    let hull: Vec<Option<(u64, u64)>> = ug
        .iter()
        .map(|g| {
            g.iter()
                .filter_map(|&x| s[x].span_ns)
                .reduce(|a, b| (a.0.min(b.0), a.1.max(b.1)))
        })
        .collect();
    let epochs_of = |g: usize| -> BTreeSet<String> {
        ug[g]
            .iter()
            .flat_map(|&x| s[x].epochs.iter().map(|e| e.epoch.clone()))
            .collect()
    };
    let mut rel = vec![vec![(Rel::No, false); n]; n];
    let mut miss: Vec<Vec<Miss>> = vec![vec![None; n]; n];
    for i in 0..n {
        for j in 0..n {
            if i == j {
                continue;
            }
            let shared = epochs_of(i).intersection(&epochs_of(j)).count() > 0;
            if !shared {
                let ov = match (hull[i], hull[j]) {
                    (Some(a), Some(b)) => a.0.max(b.0) <= a.1.min(b.1),
                    _ => false,
                };
                rel[i][j] = (if ov { Rel::Yes } else { Rel::No }, false);
                continue;
            }
            let (lo_g, hi_g) = (i.min(j), i.max(j));
            let mut yes = false;
            let mut unk = false;
            let mut first_miss = None;
            for &a in &ug[lo_g] {
                for &b in &ug[hi_g] {
                    for ea in &s[a].epochs {
                        for eb in s[b].epochs.iter().filter(|e| e.epoch == ea.epoch) {
                            let lo = ea.first_ns.max(eb.first_ns);
                            let hi = ea.last_ns.min(eb.last_ns);
                            if lo > hi {
                                continue;
                            }
                            let ra = has_row_model(c, a, lo, hi);
                            let rb = has_row_model(c, b, lo, hi);
                            // A row of each, three-valued.
                            let v = match (ra, rb) {
                                (Some(false), _) | (_, Some(false)) => Some(false),
                                (Some(true), Some(true)) => Some(true),
                                _ => None,
                            };
                            match v {
                                Some(true) => yes = true,
                                Some(false) => {
                                    if first_miss.is_none() {
                                        first_miss = Some((ea.epoch.clone(), (lo, hi)));
                                    }
                                }
                                None => unk = true,
                            }
                        }
                    }
                }
            }
            rel[i][j] = (
                if yes {
                    Rel::Yes
                } else if unk {
                    Rel::Unk
                } else {
                    Rel::No
                },
                true,
            );
            miss[i][j] = first_miss;
        }
    }
    let armed = s.iter().any(|x| x.labels.contains_key(ARM_LABEL));
    let arms: Vec<BTreeSet<String>> = ug
        .iter()
        .map(|g| {
            g.iter()
                .filter_map(|&x| s[x].labels.get(ARM_LABEL).cloned())
                .collect()
        })
        .collect();
    let mut bo = vec![vec![false; n]; n];
    for b in 0..n {
        for x in 0..n {
            bo[b][x] = if !armed {
                true
            } else if arms[b].len() == 1 {
                arms[x] == arms[b]
            } else if arms[b].is_empty() {
                !arms[x].is_empty()
            } else {
                false
            };
        }
    }
    let mut is_bridge = vec![false; n];
    for b in 0..n {
        if armed && arms[b].len() > 1 {
            continue;
        }
        'outer: for x in 0..n {
            for y in 0..n {
                if x == b || y == b || x == y {
                    continue;
                }
                if bo[b][x]
                    && bo[b][y]
                    && rel[b][x].0 == Rel::Yes
                    && rel[b][y].0 == Rel::Yes
                    && rel[x][y].0 == Rel::No
                {
                    is_bridge[b] = true;
                    break 'outer;
                }
            }
        }
    }
    let mut parent: Vec<usize> = (0..n).collect();
    fn rt(p: &mut [usize], mut i: usize) -> usize {
        while p[i] != i {
            i = p[i];
        }
        i
    }
    for i in 0..n {
        for j in i + 1..n {
            if is_bridge[i] || is_bridge[j] {
                continue;
            }
            if rel[i][j] == (Rel::Yes, true) {
                let (a, b) = (rt(&mut parent, i), rt(&mut parent, j));
                if a != b {
                    parent[a.max(b)] = a.min(b);
                }
            }
        }
    }
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut roots: Vec<usize> = Vec::new();
    for i in (0..n).filter(|&i| !is_bridge[i]) {
        let r = rt(&mut parent, i);
        let k = match roots.iter().position(|&x| x == r) {
            Some(k) => k,
            None => {
                roots.push(r);
                groups.push(vec![]);
                groups.len() - 1
            }
        };
        groups[k].extend(&ug[i]);
    }
    for g in &mut groups {
        g.sort_unstable();
    }
    let mut notices = Vec::new();
    for (i, g) in ug.iter().enumerate() {
        let hosts: BTreeSet<&String> = g
            .iter()
            .filter_map(|&x| s[x].labels.get(HOST_LABEL))
            .collect();
        if hosts.len() > 1 {
            notices.push(format!(
                "{:?}",
                Notice::HostsDiffer {
                    uuid_group: i,
                    hosts: hosts.into_iter().cloned().collect()
                }
            ));
        }
    }
    for i in 0..n {
        for j in i + 1..n {
            if is_bridge[i] || is_bridge[j] || rt(&mut parent, i) == rt(&mut parent, j) {
                continue;
            }
            match rel[i][j] {
                (Rel::No, true) => {
                    if let Some((e, x)) = miss[i][j].clone() {
                        notices.push(format!(
                            "{:?}",
                            Notice::EpochWithoutSharedRow {
                                epoch: e,
                                uuid_groups: [i, j],
                                intersection_ns: x
                            }
                        ));
                    }
                }
                // The epoch named is the first one that could not be told;
                // only the kind and the pair are compared.
                (Rel::Unk, true) => {
                    notices.push(format!("RowsUnread {{ uuid_groups: [{i}, {j}] }}"));
                }
                _ => {}
            }
        }
    }
    notices.sort();
    Brute {
        uuid_groups: ug,
        groups,
        bridges: (0..n).filter(|&b| is_bridge[b]).collect(),
        notices,
        rel,
        bridges_over: bo,
    }
}

fn normalize(n: &Notice) -> String {
    match n {
        Notice::RowsUnread { uuid_groups, .. } => {
            format!(
                "RowsUnread {{ uuid_groups: [{}, {}] }}",
                uuid_groups[0], uuid_groups[1]
            )
        }
        other => format!("{other:?}"),
    }
}

fn check(c: &Case) -> Result<(), String> {
    let b = brute(c);
    let g = form(&c.sources, &mut |s, lo, hi| has_row_model(c, s, lo, hi));
    let ug: Vec<Vec<usize>> = g.uuid_groups.iter().map(|u| u.sources.clone()).collect();
    if ug != b.uuid_groups {
        return Err(format!("uuid groups {ug:?} vs {:?}", b.uuid_groups));
    }
    let bridges: Vec<usize> = g.bridges.iter().map(|x| x.uuid_group).collect();
    if bridges != b.bridges {
        return Err(format!("bridges {bridges:?} vs brute {:?}", b.bridges));
    }
    for br in &g.bridges {
        let [x, y] = br.between;
        let k = br.uuid_group;
        let ok = x != y
            && x != k
            && y != k
            && b.bridges_over[k][x]
            && b.bridges_over[k][y]
            && b.rel[k][x].0 == Rel::Yes
            && b.rel[k][y].0 == Rel::Yes
            && b.rel[x][y].0 == Rel::No;
        if !ok {
            return Err(format!("bad witness {br:?}"));
        }
    }
    let groups: Vec<Vec<usize>> = g.groups.iter().map(|p| p.sources.clone()).collect();
    if groups != b.groups {
        return Err(format!("groups {groups:?} vs brute {:?}", b.groups));
    }
    for p in &g.groups {
        if p.joins.len() + 1 != p.uuid_groups.len() {
            return Err(format!("joins count {p:?}"));
        }
        let mut par: BTreeMap<usize, usize> = p.uuid_groups.iter().map(|&u| (u, u)).collect();
        fn r(p: &mut BTreeMap<usize, usize>, mut i: usize) -> usize {
            while p[&i] != i {
                i = p[&i];
            }
            i
        }
        for j in &p.joins {
            let [x, y] = j.uuid_groups;
            if x >= y || !par.contains_key(&x) || !par.contains_key(&y) {
                return Err(format!("join outside group {j:?}"));
            }
            if b.rel[x][y] != (Rel::Yes, true) {
                return Err(format!("join not a join {j:?}"));
            }
            let [sa, sb] = j.sources;
            let (lo, hi) = j.overlap_ns;
            if has_row_model(c, sa, lo, hi) != Some(true)
                || has_row_model(c, sb, lo, hi) != Some(true)
            {
                return Err(format!("join without rows {j:?}"));
            }
            if !g.uuid_groups[x].sources.contains(&sa) || !g.uuid_groups[y].sources.contains(&sb) {
                return Err(format!("join sources {j:?}"));
            }
            let (ra, rb) = (r(&mut par, x), r(&mut par, y));
            if ra == rb {
                return Err(format!("join cycle {j:?}"));
            }
            par.insert(ra.max(rb), ra.min(rb));
        }
    }
    let mut notices: Vec<String> = g.notices.iter().map(normalize).collect();
    notices.sort();
    if notices != b.notices {
        return Err(format!("notices {notices:?}\n vs brute {:?}", b.notices));
    }
    Ok(())
}

/// Check `cases` seeded cases of `nmin..=nmax` sources over `domain` grid
/// steps; the failures, and how many cases had each feature (so a change to
/// the generator that stops exercising one shows).
fn run(cases: u64, nmin: usize, nmax: usize, domain: u64) -> Vec<(u64, usize, String)> {
    let mut fails = Vec::new();
    let (mut nb, mut nfb, mut nmulti, mut nunread, mut nmiss, mut nmixed) = (0, 0, 0, 0, 0, 0);
    for seed in 0..cases {
        let n = nmin + (seed as usize % (nmax - nmin + 1));
        let c = gen(seed.wrapping_mul(2654435761) ^ 0xABCDEF, n, domain);
        let g = form(&c.sources, &mut |s, lo, hi| has_row_model(&c, s, lo, hi));
        if !g.bridges.is_empty() {
            nb += 1;
        }
        if g.bridges.iter().any(|b| {
            let [x, y] = b.between;
            match (g.uuid_groups[x].span_ns, g.uuid_groups[y].span_ns) {
                (Some(a), Some(bb)) => a.0.max(bb.0) <= a.1.min(bb.1),
                _ => false,
            }
        }) {
            nfb += 1;
        }
        if g.groups.iter().any(|p| p.uuid_groups.len() > 1) {
            nmulti += 1;
        }
        if g.notices
            .iter()
            .any(|n| matches!(n, Notice::RowsUnread { .. }))
        {
            nunread += 1;
        }
        if g.notices
            .iter()
            .any(|n| matches!(n, Notice::EpochWithoutSharedRow { .. }))
        {
            nmiss += 1;
        }
        if g.uuid_groups.iter().any(|u| {
            u.sources
                .iter()
                .filter_map(|&s| c.sources[s].labels.get("arm"))
                .collect::<BTreeSet<_>>()
                .len()
                > 1
        }) {
            nmixed += 1;
        }
        if let Err(e) = check(&c) {
            fails.push((seed, n, e));
        }
    }
    println!("cases with: bridge {nb}, bridge via intersecting-hull pair {nfb}, multi-uuid-group process group {nmulti}, RowsUnread {nunread}, EpochWithoutSharedRow {nmiss}, mixed-arm group {nmixed}");
    fails
}

#[test]
fn form_matches_the_reference_on_small_cases() {
    let fails = run(5000, 2, 8, 20);
    for (seed, n, e) in fails.iter().take(5) {
        println!("seed {seed} n {n}: {e}");
    }
    assert!(fails.is_empty(), "{} of 5000 failed", fails.len());
}

#[test]
fn form_matches_the_reference_on_fifty_sources() {
    let fails = run(60, 50, 50, 60);
    for (seed, n, e) in fails.iter().take(3) {
        println!("seed {seed} n {n}: {e}");
    }
    assert!(fails.is_empty(), "{} of 60 failed", fails.len());
}

/// One case, printed for minimising a failure:
/// `EQ_SEED=<seed> EQ_N=<n> [EQ_DOMAIN=<d>] cargo test -p metriken-storage
/// --all-features show_case -- --ignored --nocapture`.
#[test]
#[ignore]
fn show_case() {
    let seed: u64 = std::env::var("EQ_SEED").unwrap().parse().unwrap();
    let n: usize = std::env::var("EQ_N").unwrap().parse().unwrap();
    let domain: u64 = std::env::var("EQ_DOMAIN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let c = gen(seed.wrapping_mul(2654435761) ^ 0xABCDEF, n, domain);
    for (i, s) in c.sources.iter().enumerate() {
        let epochs: Vec<_> = s
            .epochs
            .iter()
            .map(|e| (&e.epoch, e.first_ns, e.last_ns))
            .collect();
        println!(
            "{i}: uuid {:?} labels {:?} span {:?} epochs {epochs:?} rows {:?} unreadable {:?}",
            s.uuid, s.labels, s.span_ns, c.rows[i], c.unreadable[i]
        );
    }
    println!("random_rows {}", c.random_rows);
    println!("{:?}", check(&c));
}

/// A row check that changes its answer between calls, as on a live archive
/// whose WAL was sealed while grouping: every first check finds a row, every
/// later one cannot tell. X 0–100 bridges A 10–20 and C 80–90; A and B
/// 12–22 are joined through X in the first sweep only, so the second sweep
/// checks them again, cannot tell, and reports them.
#[test]
fn an_answer_that_changes_between_sweeps_is_reported() {
    let source = |span: (u64, u64)| {
        let metadata: BTreeMap<String, String> =
            [(PRODUCER_EPOCH.to_string(), "e".to_string())].into();
        SourceInfo::new(BTreeMap::new(), 0, None, Some(span), &metadata)
    };
    let s = vec![
        source((0, 100)),
        source((10, 20)),
        source((12, 22)),
        source((80, 90)),
    ];
    let mut seen: HashMap<(usize, u64, u64), u32> = HashMap::new();
    let g = form(&s, &mut |src, lo, hi| {
        let calls = seen.entry((src, lo, hi)).or_default();
        *calls += 1;
        (*calls == 1).then_some(true)
    });
    let bridges: Vec<usize> = g.bridges.iter().map(|b| b.uuid_group).collect();
    assert_eq!(bridges, vec![0]);
    let groups: Vec<Vec<usize>> = g.groups.iter().map(|p| p.sources.clone()).collect();
    assert_eq!(groups, vec![vec![1], vec![2], vec![3]]);
    assert_eq!(
        g.notices,
        vec![Notice::RowsUnread {
            epoch: "e".into(),
            uuid_groups: [1, 2],
        }]
    );
}

/// Review's minimal case for the three-valued AND: one source cannot tell,
/// the other has no row, so they share no row and that is what is reported.
#[test]
fn no_row_in_one_source_decides_whatever_the_other() {
    let source = || {
        let metadata: BTreeMap<String, String> =
            [(PRODUCER_EPOCH.to_string(), "e0".to_string())].into();
        SourceInfo::new(BTreeMap::new(), 0, None, Some((60, 109)), &metadata)
    };
    let s = vec![source(), source()];
    let g = form(&s, &mut |src, _, _| match src {
        0 => None,
        _ => Some(false),
    });
    assert_eq!(
        g.notices,
        vec![Notice::EpochWithoutSharedRow {
            epoch: "e0".into(),
            uuid_groups: [0, 1],
            intersection_ns: (60, 109),
        }]
    );
}
