use super::parser::{Contact, Phase};

const DOWN: u32 = 0;
const UP: u32 = 1;
const MOVE: u32 = 2;
const CANCEL: u32 = 3;
const POINTER_DOWN: u32 = 5;
const POINTER_UP: u32 = 6;

/// A touch as Android itself reports it: what happened, to which of the fingers, and where
/// every finger is, in display pixels. It is the source of touches on systems that have no
/// touch devices to read, such as Android in a container.
#[derive(Clone, Debug, PartialEq)]
pub struct Motion {
    pub action: u32,
    pub index: usize,
    pub pointers: Vec<(u32, [f32; 2])>,
}

impl Motion {
    /// Reads the helper's line: `touch <action> <index>` and then `<id> <x> <y>` for each finger.
    pub fn parse(line: &str) -> Option<Self> {
        let mut fields = line.strip_prefix("touch ")?.split_ascii_whitespace();
        let (action, index) = (fields.next()?.parse().ok()?, fields.next()?.parse().ok()?);
        let mut pointers = Vec::new();
        while let Some(id) = fields.next() {
            pointers.push((id.parse().ok()?, [fields.next()?.parse().ok()?, fields.next()?.parse().ok()?]));
        }
        Some(Self { action, index, pointers })
    }

    /// What the motion means for each finger.
    pub fn contacts(&self) -> impl Iterator<Item = (Contact, [f32; 2])> + '_ {
        (0usize..).zip(&self.pointers).filter_map(|(index, &(id, position))| {
            let phase = match self.action {
                DOWN | POINTER_DOWN if index == self.index => Phase::Start,
                UP | POINTER_UP if index == self.index => Phase::End,
                DOWN | POINTER_DOWN | UP | POINTER_UP | MOVE => Phase::Move,
                CANCEL => Phase::Cancel,
                _ => return None,
            };
            Some((Contact { id, phase, u: 0.0, v: 0.0 }, position))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phases(line: &str) -> Vec<(u32, Phase, [f32; 2])> {
        Motion::parse(line).expect("a touch line").contacts().map(|(contact, position)| (contact.id, contact.phase, position)).collect()
    }

    #[test]
    fn lines_from_the_helper_become_contacts() {
        assert_eq!(phases("touch 0 0 3 120.5 640.0"), [(3, Phase::Start, [120.5, 640.0])]);
        assert_eq!(phases("touch 5 1 3 121.0 640.0 4 900.0 200.25"), [(3, Phase::Move, [121.0, 640.0]), (4, Phase::Start, [900.0, 200.25])]);
        assert_eq!(phases("touch 2 0 3 130.0 650.0 4 905.0 210.0"), [(3, Phase::Move, [130.0, 650.0]), (4, Phase::Move, [905.0, 210.0])]);
        assert_eq!(phases("touch 6 0 3 130.0 650.0 4 905.0 210.0"), [(3, Phase::End, [130.0, 650.0]), (4, Phase::Move, [905.0, 210.0])]);
        assert_eq!(phases("touch 3 0 4 905.0 210.0"), [(4, Phase::Cancel, [905.0, 210.0])]);
        assert!(phases("touch 7 0 4 905.0 210.0").is_empty());
        assert_eq!((Motion::parse("ready"), Motion::parse("touch 0 0 3 12.0")), (None, None));
    }
}
