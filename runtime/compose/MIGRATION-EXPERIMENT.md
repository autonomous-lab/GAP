# Historical cold-migration experiment

The initial isolated experiment transferred a disposable 1-vCPU, 1024-MiB,
8-GiB VM from node01 to node02. It flattened the stopped qcow2 disk, verified its
SHA256 after transfer, booted the destination and read a guest witness file.
A fresh source controller refused to restart the fenced source. Both disposable
environments and their keys were removed. No existing customer VM was moved.

That experiment established disk-transfer feasibility; it did not itself deliver
routing, accounting or the dashboard. The integrated implementation and its
operator/API requirements are documented in [MIGRATION.md](MIGRATION.md).
