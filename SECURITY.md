# Security and release practices

Report suspected vulnerabilities through GitHub's private vulnerability reporting when
available. Include the affected version, expected behavior, observed behavior and
minimal reproduction with synthetic data. Keep secrets and live connection details in
your own workspace.

## Contributions from private workflows

Describe the reusable capability, then implement and test it against synthetic fixtures.
Keep private project names, code, Git references, board profiles, symbols, device
identifiers, local paths, logs, screenshots and firmware binaries in the private workspace.
Use fictional target names such as `lab-a` in the repository.

Before release, review tracked files, file names, the Git history and refs being published,
rendered media, and the contents of each built package. Apply any private-name checks
locally. Keep the list of private terms in the private workspace.

## Remote operation and verification

- The device generates its OpenOCD Link key at installation. Treat its key file as a
  credential and transfer it through a trusted channel.
- OpenOCD RPC provides privileged access. Keep it on loopback or behind the authenticated
  tunnel. Check the current target identity before firmware operations.
- Show the target and expected effects before hardware changes. Record an interrupted
  operation with an uncertain outcome and inspect state before repeating it.
- State what each release has exercised through host tests, simulated sessions and
  physical hardware. Include the tested configuration and operation in each claim.
