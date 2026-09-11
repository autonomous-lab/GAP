//! Durable operator decisions, independent of whether the admin UI is enabled.
use serde::{Deserialize,Serialize};

#[derive(Clone,Debug,Serialize,Deserialize)]
pub struct Decision {
    pub generation:u64,
    pub active:bool,
    pub reason:String,
    pub actor:String,
    pub at:u64,
}

#[derive(Clone,Debug,Serialize,Deserialize)]
pub struct Record {
    pub current:Decision,
    /// Kept in the same atomic state record as the decision, so a suspension
    /// cannot take effect without its attribution and decision history.
    pub history:Vec<Decision>,
}

impl Record {
    pub fn next(previous:Option<&Self>,expected:u64,active:bool,reason:&str,actor:&str,at:u64)->crate::Result<Self> {
        let generation=previous.map_or(0,|r|r.current.generation);
        if generation!=expected {return Err(crate::Error::Other("suspension_version_changed_refresh_before_retry".into()))}
        if reason.trim().is_empty() || reason.len()>2000 {return Err(crate::Error::Other("suspension_reason_required".into()))}
        let next=generation.checked_add(1).ok_or_else(||crate::Error::Other("suspension_generation_exhausted".into()))?;
        let current=Decision{generation:next,active,reason:reason.trim().into(),actor:actor.into(),at};
        let mut history=previous.map_or_else(Vec::new,|r|r.history.clone());
        history.push(current.clone());
        Ok(Self{current,history})
    }
}

#[cfg(test)] mod tests {
    use super::*;
    #[test] fn decisions_preserve_history_and_reject_stale_changes() {
        let first=Record::next(None,0,true,"abuse","admin@example.com",100).unwrap();
        assert!(Record::next(Some(&first),0,false,"resolved","admin@example.com",101).is_err());
        let second=Record::next(Some(&first),1,false,"resolved","admin@example.com",102).unwrap();
        assert!(!second.current.active);assert_eq!(second.current.generation,2);
        assert_eq!(second.history.len(),2);assert!(second.history[0].active);
        assert!(Record::next(Some(&second),2,true," ","admin@example.com",103).is_err());
    }
}
