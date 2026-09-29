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

#[cfg(test)]
mod tests {
    use super::super::index::NodeKind;
    use super::*;

    /// A node at file position `at` (offset = at), uuid `id`.
    fn node(at: u64, id: u128, parent: u128, ts: u64, kind: NodeKind) -> Node {
        Node { offset: at, len: 1, kind, ts, uuid: id, parent }
    }

    fn line(nodes: &[Node]) -> Vec<u128> {
        let t = Tree::new(nodes);
        t.main_line().into_iter().map(|i| nodes[i].uuid).collect()
    }

    use NodeKind::{Assistant as A, Other as O, User as U};

    #[test]
    fn a_rewind_branches_and_the_newest_tip_wins() {
        // 1 → 2 → 3 → 4 (abandoned) ; 2 → 5 → 6 (rewound to 2, asked again)
        let nodes = [
            node(0, 1, 0, 10, U),
            node(1, 2, 1, 20, A),
            node(2, 3, 2, 30, U),
            node(3, 4, 3, 40, A),
            node(4, 5, 2, 50, U),
            node(5, 6, 5, 60, A),
        ];
        assert_eq!(line(&nodes), [1, 2, 5, 6]);
        // Equal stamps: the later line wins.
        let tied = [node(0, 1, 0, 10, U), node(1, 2, 1, 20, A), node(2, 3, 1, 20, A)];
        assert_eq!(line(&tied), [1, 3]);
        assert!(line(&[node(0, 1, 0, 1, O)]).is_empty(), "no conversation, no main line");
    }

    #[test]
    fn side_notes_never_end_the_main_line() {
        // A tool call (2) with progress notes under it newer than its result
        // (4), and a turn-duration note after the answer: the line runs
        // through the conversation and stops at its last node.
        let nodes = [
            node(0, 1, 0, 10, U),
            node(1, 2, 1, 20, A),
            node(2, 3, 2, 21, O),
            node(3, 4, 2, 22, U),
            node(4, 7, 3, 99, O),
            node(5, 5, 4, 30, A),
            node(6, 6, 5, 31, O),
        ];
        assert_eq!(line(&nodes), [1, 2, 4, 5]);
        // Notes in the chain itself (a prompt parented on the note) stay in it.
        let chained = [node(0, 1, 0, 10, U), node(1, 2, 1, 20, A), node(2, 3, 2, 21, O), node(3, 4, 3, 30, U)];
        assert_eq!(line(&chained), [1, 2, 3, 4]);
    }

    #[test]
    fn a_compaction_gap_is_bridged_to_the_tip_before_it() {
        // 1 → 2 → 3, then a boundary (4) whose logical parent (99) was never
        // written, and the conversation going on under it.
        let nodes = [
            node(0, 1, 0, 10, U),
            node(1, 2, 1, 20, A),
            node(2, 8, 1, 15, A), // an abandoned branch, older
            node(3, 3, 2, 30, U),
            node(4, 4, 99, 40, O),
            node(5, 5, 4, 50, U),
            node(6, 6, 5, 60, A),
        ];
        assert_eq!(line(&nodes), [1, 2, 3, 4, 5, 6]);
        // A file that starts mid-conversation: nothing before to bridge to.
        assert_eq!(line(&[node(0, 5, 42, 1, U), node(1, 6, 5, 2, A)]), [5, 6]);
    }

    #[test]
    fn repeated_uuids_and_cycles_do_not_confuse_it() {
        let dup = [node(0, 1, 0, 10, U), node(1, 2, 1, 20, A), node(2, 1, 0, 30, U)];
        assert_eq!(line(&dup), [1, 2]);
        // Every node of a parent cycle has conversation below it, so there
        // is no tip; what matters is that the walk terminates.
        let cycle = [node(0, 1, 2, 10, U), node(1, 2, 1, 20, A), node(2, 3, 3, 30, U)];
        assert!(line(&cycle).len() <= 3);
    }

    #[test]
    fn a_cursor_continues_after_its_tip_or_from_the_fork() {
        let nodes = [
            node(0, 1, 0, 10, U),
            node(1, 2, 1, 20, A),
            node(2, 3, 2, 30, U),
            node(3, 4, 3, 40, A),
            node(4, 5, 2, 50, U),
            node(5, 6, 5, 60, A),
        ];
        let t = Tree::new(&nodes);
        let main = t.main_line(); // 1 2 5 6
        assert_eq!(t.resume(&main, 2), Resume::After(2));
        assert_eq!(t.resume(&main, 6), Resume::After(4), "nothing new");
        // The reader last saw 4, on the branch since abandoned: it forked at 2.
        assert_eq!(t.resume(&main, 4), Resume::Rewound { fork: Some(2), from: 2 });
        assert_eq!(t.resume(&main, 77), Resume::Rewound { fork: None, from: 0 });
    }
}
