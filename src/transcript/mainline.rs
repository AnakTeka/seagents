//! The transcript is a tree, not a log (SESSIONS-SPEC "The transcript is a
//! tree"): every rewind, edit or retry starts a branch off an earlier node,
//! so the file in line order interleaves abandoned attempts with the live
//! conversation (one real 38 MB session: 20 branch points, 22 tips). What
//! is shown is the **main line** — root to the newest tip.
//!
//! "Tip" means the newest *conversation* node with no conversation below
//! it, not the newest leaf of any kind. Measured over this machine's
//! transcripts, ~1,100 leaves are side notes hung off the conversation — a
//! `progress` record under a running tool call (its result becomes the
//! call's sibling), an `attachment`, a `system` note — and 94 of them are
//! newer than the sibling the conversation went on through. Picking the
//! newest leaf of any kind would end the main line on a side note mid-turn,
//! and a cursor left there would read as a rewind once the turn went on.
//!
//! A compaction whose pre-compaction tip is not in the file (the
//! boundary's `logicalParentUuid` names a line never written — 5 of 15 here)
//! leaves the line rooted at the boundary; the walk bridges to the newest
//! tip written before the boundary and carries on from there, so the main
//! line still runs from the conversation's real start.

use std::collections::HashMap;

use super::index::Node;

/// The nodes of one index as a tree. A uuid that repeats within the file is
/// its first occurrence.
pub(crate) struct Tree<'a> {
    nodes: &'a [Node],
    by_uuid: HashMap<u128, usize>,
}

/// Where a continuing read picks up, given the previous read's tip.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Resume {
    /// The main line still runs through the tip: from this position on.
    After(usize),
    /// The main line left the old one at `fork` (None: no common node at
    /// all — the tip is not in this file); from position `from` on.
    Rewound { fork: Option<u128>, from: usize },
}

impl<'a> Tree<'a> {
    pub(crate) fn new(nodes: &'a [Node]) -> Tree<'a> {
        let mut by_uuid = HashMap::with_capacity(nodes.len());
        for (i, n) in nodes.iter().enumerate() {
            by_uuid.entry(n.uuid).or_insert(i);
        }
        Tree { nodes, by_uuid }
    }

    fn parent(&self, i: usize) -> Option<usize> {
        let p = self.nodes[i].parent;
        (p != 0).then(|| self.by_uuid.get(&p).copied()).flatten()
    }

    fn canonical(&self, i: usize) -> bool {
        self.by_uuid.get(&self.nodes[i].uuid) == Some(&i)
    }

    /// The main line, root first, as indices into the nodes; empty when
    /// there is no conversation at all. Ends at the tip, always a
    /// conversation node.
    pub(crate) fn main_line(&self) -> Vec<usize> {
        let n = self.nodes.len();
        // Mark every node with a conversation node below it; each is marked
        // once, so this is linear even on a deep chain.
        let mut above_conversation = vec![false; n];
        for i in (0..n).filter(|&i| self.nodes[i].kind.conversation() && self.canonical(i)) {
            let mut p = self.parent(i);
            while let Some(j) = p.filter(|&j| !above_conversation[j]) {
                above_conversation[j] = true;
                p = self.parent(j);
            }
        }
        let tips: Vec<usize> = (0..n)
            .filter(|&i| self.nodes[i].kind.conversation() && self.canonical(i) && !above_conversation[i])
            .collect();
        let newest = |bound: u64, visited: &[bool]| {
            tips.iter()
                .copied()
                .filter(|&t| self.nodes[t].offset < bound && !visited[t])
                .max_by_key(|&t| (self.nodes[t].ts, self.nodes[t].offset))
        };

        let mut path = Vec::new();
        let mut visited = vec![false; n];
        let mut next = newest(u64::MAX, &visited);
        while let Some(start) = next {
            let mut i = start;
            loop {
                if visited[i] {
                    // A parent cycle in a malformed file: stop here.
                    path.reverse();
                    return path;
                }
                visited[i] = true;
                path.push(i);
                match self.parent(i) {
                    Some(p) => i = p,
                    None => break,
                }
            }
            // A root. One that names a parent the file lacks is a gap in the
            // conversation (a compaction), bridged to what came before it.
            next = (self.nodes[i].parent != 0)
                .then(|| newest(self.nodes[i].offset, &visited))
                .flatten();
        }
        path.reverse();
        path
    }

    /// Where to continue after a read whose tip was `leaf`, over the current
    /// main line `main`.
    pub(crate) fn resume(&self, main: &[usize], leaf: u128) -> Resume {
        let pos: HashMap<u128, usize> = main.iter().enumerate().map(|(p, &i)| (self.nodes[i].uuid, p)).collect();
        if let Some(p) = pos.get(&leaf) {
            return Resume::After(p + 1);
        }
        let mut seen = vec![false; self.nodes.len()];
        let mut at = self.by_uuid.get(&leaf).copied();
        while let Some(i) = at.filter(|&i| !seen[i]) {
            seen[i] = true;
            if let Some(p) = pos.get(&self.nodes[i].uuid) {
                return Resume::Rewound { fork: Some(self.nodes[i].uuid), from: p + 1 };
            }
            at = self.parent(i);
        }
        Resume::Rewound { fork: None, from: 0 }
    }
}
