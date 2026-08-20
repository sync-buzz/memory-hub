# memory-hub-index

Disposable LanceDB projection of generic Memory Hub records. `Projection` is
the only public storage seam: it rebuilds from an immutable canonical snapshot,
applies exact snapshot diffs, and refuses reads while the projection is not
fresh. The index directory may always be deleted and reconstructed from Git.
