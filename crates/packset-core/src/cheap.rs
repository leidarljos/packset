//! Cheap-model schedule. Extract runs on compaction only.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheapJob {
    Extract,
    Fidelity,
    LinkRewrite,
    DueSuggest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheapWhen {
    Compaction,
    OnDemand,
}

/// Compaction owns extract and link rewrite. On-demand owns fidelity and due.
pub fn allowed(job: CheapJob, when: CheapWhen) -> bool {
    matches!(
        (job, when),
        (CheapJob::Extract, CheapWhen::Compaction)
            | (CheapJob::LinkRewrite, CheapWhen::Compaction)
            | (CheapJob::Fidelity, CheapWhen::OnDemand)
            | (CheapJob::DueSuggest, CheapWhen::OnDemand)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_only_on_compaction() {
        assert!(allowed(CheapJob::Extract, CheapWhen::Compaction));
        assert!(!allowed(CheapJob::Extract, CheapWhen::OnDemand));
        assert!(!allowed(CheapJob::Fidelity, CheapWhen::Compaction));
        assert!(allowed(CheapJob::Fidelity, CheapWhen::OnDemand));
    }
}
