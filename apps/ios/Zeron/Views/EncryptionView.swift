import SwiftUI

/// Encrypted sync on the phone (RFC 0001 §4.2): the vault state, the
/// comparison code while an approval is pending, and the two ways in —
/// approval from an already-approved device, or the recovery kit. The
/// vault fingerprint typed here comes from the approving device (Settings →
/// Encryption → "Copy vault fingerprint", or `zeron vault status`) and pins
/// the genesis so a relay cannot hand this phone a substitute vault.
struct EncryptionView: View {
    var requiresApproval = false
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @State private var fingerprint = ""
    @State private var kit = ""
    @State private var error: String?
    @State private var working = false

    private var status: MobileVaultStatus? { model.vaultStatus }

    var body: some View {
        NavigationStack {
            List {
                Section {
                    LabeledContent("Status", value: statusTitle)
                    Text(statusCopy)
                        .font(.footnote)
                        .foregroundStyle(.secondary)
                    if let fingerprint = status?.fingerprint {
                        LabeledContent("Vault fingerprint") {
                            Text(fingerprint.prefix(16) + "…")
                                .font(.system(.footnote, design: .monospaced))
                                .textSelection(.enabled)
                        }
                    }
                    if let epoch = status?.epoch {
                        LabeledContent("Key epoch", value: String(epoch))
                    }
                } header: {
                    Text("End-to-end encryption")
                }

                if let code = status?.pairingCode, status?.phase == .pending {
                    Section {
                        Text(code)
                            .font(.system(size: 34, weight: .semibold, design: .monospaced))
                            .frame(maxWidth: .infinity)
                            .padding(.vertical, 8)
                        Text("On the approving device, approve only if it shows exactly this code.")
                            .font(.footnote)
                            .foregroundStyle(.secondary)
                    } header: {
                        Text("Comparison code")
                    }
                }

                if canEnroll {
                    Section {
                        TextField("Vault fingerprint (64 hex characters)", text: $fingerprint)
                            .font(.system(.footnote, design: .monospaced))
                            .textInputAutocapitalization(.never)
                            .autocorrectionDisabled()
                        Button("Approve from another device") { run { try await model.enrollVault(fingerprintHex: fingerprint) } }
                            .disabled(working || fingerprint.count < 64)
                    } header: {
                        Text("Approve this device")
                    } footer: {
                        Text("Paste the vault fingerprint shown on an approved device. That device then compares the code above before approving. An approved device can read all synced content and manage devices.")
                    }

                    Section {
                        TextField("Recovery key (XXXXX-XXXXX-…)", text: $kit)
                            .font(.system(.footnote, design: .monospaced))
                            .textInputAutocapitalization(.characters)
                            .autocorrectionDisabled()
                        Button("Use recovery key") { run { try await model.recoverVault(kit: kit, fingerprintHex: fingerprint) } }
                            .disabled(working || kit.count < 55 || fingerprint.count < 64)
                    } header: {
                        Text("Recovery")
                    } footer: {
                        Text("Enter the recovery key and the vault fingerprint from your recovery file. This adds the phone under a fresh key epoch; other devices catch up automatically.")
                    }
                }

                if let error {
                    Section {
                        Text(error).foregroundStyle(.red)
                    }
                }
            }
            .navigationTitle("Encryption")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .topBarLeading) {
                    if requiresApproval {
                        Button("Sign out") { model.signOut() }
                    } else {
                        Button("Done") { dismiss() }
                    }
                }
                ToolbarItem(placement: .topBarTrailing) {
                    Button {
                        model.refreshVault()
                    } label: {
                        if model.vaultBusy || working {
                            ProgressView()
                        } else {
                            Image(systemName: "arrow.clockwise")
                        }
                    }
                    .disabled(model.vaultBusy || working)
                }
            }
        }
        .onAppear {
            if fingerprint.isEmpty, let known = status?.fingerprint { fingerprint = known }
            model.refreshVault()
        }
        .task(id: status?.phase) {
            guard status?.phase == .pending || status?.phase == .keyUpdateRequired else { return }
            while !Task.isCancelled {
                do { try await Task.sleep(for: .seconds(2)) } catch { return }
                model.refreshVault()
            }
        }
    }

    private var canEnroll: Bool {
        switch status?.phase {
        case .notEnrolled, .revoked, .legacy, .pending, nil: return true
        default: return false
        }
    }

    private var statusTitle: String {
        switch status?.phase {
        case .ready: return "Encrypted"
        case .legacy: return "Not set up"
        case .notEnrolled: return "Approve this device"
        case .pending: return "Waiting for approval"
        case .locked: return "Locked"
        case .keyUpdateRequired: return "Waiting for keys"
        case .verificationFailed: return "Sync paused"
        case .revoked: return "This device was removed"
        case .recoveryConfirmationRequired: return "Confirm recovery kit"
        case .checking, nil: return "Checking…"
        }
    }

    private var statusCopy: String {
        if let message = status?.message { return message }
        switch status?.phase {
        case .ready:
            return "Synced content is encrypted on your devices. Only approved devices, or someone with your recovery key, can read it."
        case .legacy:
            return "This account has no encrypted vault. Set one up on a desktop (Settings → Encryption or `zeron vault setup`), then approve this phone."
        case .revoked:
            return "This phone no longer has access to encrypted sync. Ask an approved device to approve it again, or use your recovery key."
        case .notEnrolled:
            return "This account uses end-to-end encryption. Nothing syncs to this phone until an approved device admits it."
        case .pending:
            return "Open Settings → Encryption on an approved device and compare the code."
        case .locked:
            return "Secure key storage is unavailable on this phone. Existing data was retained."
        case .keyUpdateRequired:
            return "Another device changed the vault's keys; sync resumes once the new keys arrive."
        case .verificationFailed:
            return "Data from the sync backend could not be verified. Sync stays paused."
        case .recoveryConfirmationRequired:
            return "Confirm the recovery kit on the device that created the vault."
        case .checking, nil:
            return "Checking the vault for this account."
        }
    }

    private func run(_ operation: @escaping () async throws -> Void) {
        error = nil
        working = true
        Task {
            do { try await operation() } catch { self.error = error.localizedDescription }
            working = false
        }
    }
}

/// The everyday approval flow. Protocol details stay in the optional settings sheet.
struct DeviceApprovalView: View {
    @Environment(AppModel.self) private var model
    @State private var fingerprint = ""
    @State private var recoveryKey = ""
    @State private var working = false
    @State private var error: String?
    @State private var showRecovery = false
    @State private var showDetails = false

    private var pending: Bool { model.vaultStatus?.phase == .pending }
    private var finishing: Bool { model.vaultStatus?.phase == .keyUpdateRequired }
    private var knownFingerprint: String { model.vaultStatus?.fingerprint ?? fingerprint.trimmingCharacters(in: .whitespacesAndNewlines) }

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(spacing: 28) {
                    Image(systemName: pending ? "laptopcomputer" : "iphone.and.arrow.forward")
                        .font(.system(size: 44, weight: .light))
                        .foregroundStyle(Theme.accent)
                        .frame(width: 108, height: 108)
                        .background(Theme.accent.opacity(0.12), in: RoundedRectangle(cornerRadius: 30))
                        .accessibilityHidden(true)

                    VStack(spacing: 12) {
                        Text(finishing ? "Getting your chats ready" : pending ? "Check your desktop" : "Get approval to\nview your chats")
                            .font(.system(.largeTitle, design: .rounded, weight: .semibold))
                            .multilineTextAlignment(.center)
                        Text(finishing ? "Your device has been approved. This will only take a moment." : pending ? "Open the approval request on your desktop and make sure these numbers match." : "Your chats are private. Use a device you’ve already approved to let this iPhone access them.")
                            .font(.body)
                            .foregroundStyle(Theme.textMuted)
                            .multilineTextAlignment(.center)
                            .fixedSize(horizontal: false, vertical: true)
                    }

                    if pending, let code = model.vaultStatus?.pairingCode {
                        VStack(spacing: 16) {
                            Text("MATCH THESE NUMBERS")
                                .font(.caption.weight(.medium)).tracking(1.5)
                                .foregroundStyle(Theme.textMuted)
                            Text(String(code.prefix(4)) + " " + String(code.dropFirst(4)))
                                .font(.system(size: 36, weight: .medium, design: .monospaced))
                                .minimumScaleFactor(0.7).lineLimit(1)
                                .accessibilityLabel("Comparison code")
                                .accessibilityValue(code.map(String.init).joined(separator: " "))
                            HStack(spacing: 8) {
                                ProgressView().controlSize(.small)
                                Text("Waiting for approval…").font(.footnote)
                            }.foregroundStyle(Theme.textMuted)
                        }
                        .frame(maxWidth: .infinity).padding(24)
                        .background(Theme.surfaceRaised, in: RoundedRectangle(cornerRadius: 24))
                    } else if finishing {
                        ProgressView()
                    } else {
                        if model.vaultStatus?.fingerprint == nil {
                            VStack(alignment: .leading, spacing: 10) {
                                Text("Connect to your desktop").font(.headline)
                                Text("On your desktop, open Settings → Encryption and copy the vault fingerprint. Paste it here once to connect securely.")
                                    .font(.footnote).foregroundStyle(Theme.textMuted)
                                TextField("Paste connection fingerprint", text: $fingerprint, axis: .vertical)
                                    .textInputAutocapitalization(.never).autocorrectionDisabled()
                                    .font(.system(.footnote, design: .monospaced))
                                    .padding(12).background(Theme.surfaceRaised, in: RoundedRectangle(cornerRadius: 12))
                            }
                        }
                        Button { run { try await model.enrollVault(fingerprintHex: knownFingerprint) } } label: {
                            HStack(spacing: 8) {
                                if working { ProgressView().tint(.black) }
                                Text("Ask for approval").font(.headline)
                            }.frame(maxWidth: .infinity).padding(.vertical, 17)
                        }
                        .buttonStyle(.plain).foregroundStyle(.black)
                        .background(.white, in: Capsule())
                        .disabled(working || knownFingerprint.count != 64)
                        .opacity(working || knownFingerprint.count != 64 ? 0.5 : 1)
                    }
                    if let error {
                        Text(error).font(.footnote).foregroundStyle(Theme.danger)
                            .multilineTextAlignment(.center)
                    }
                    Button("Use a recovery key instead") { showRecovery = true }
                        .font(.subheadline).foregroundStyle(Theme.textMuted)
                        .disabled(working)
                    Label("Your chats stay end-to-end encrypted", systemImage: "lock.fill")
                        .font(.caption).foregroundStyle(Theme.textFaint)
                }
                .frame(maxWidth: 420).padding(.horizontal, 28).padding(.top, 48).padding(.bottom, 32)
                .frame(maxWidth: .infinity)
            }
            .background(Theme.bg.ignoresSafeArea())
            .toolbar {
                ToolbarItem(placement: .topBarLeading) {
                    Button("Sign out") { model.signOut() }.foregroundStyle(Theme.textMuted)
                }
                ToolbarItem(placement: .topBarTrailing) {
                    Button { showDetails = true } label: { Image(systemName: "info.circle") }
                        .accessibilityLabel("Connection details").foregroundStyle(Theme.textMuted)
                }
            }
            .sheet(isPresented: $showDetails) { EncryptionView() }
            .sheet(isPresented: $showRecovery) {
                NavigationStack {
                    Form {
                        Section {
                            Text("Use the recovery key you saved when you first set up private chats.")
                            TextField("Recovery key", text: $recoveryKey, axis: .vertical)
                                .textInputAutocapitalization(.characters).autocorrectionDisabled()
                            if model.vaultStatus?.fingerprint == nil {
                                TextField("Fingerprint from your recovery file", text: $fingerprint)
                                    .textInputAutocapitalization(.never).autocorrectionDisabled()
                            }
                            Button("Restore access") {
                                run { try await model.recoverVault(kit: recoveryKey, fingerprintHex: knownFingerprint) }
                            }.disabled(working || recoveryKey.count < 55 || knownFingerprint.count != 64)
                            if let error { Text(error).foregroundStyle(Theme.danger) }
                        }
                    }
                    .navigationTitle("Restore your chats").navigationBarTitleDisplayMode(.inline)
                    .toolbar { ToolbarItem(placement: .topBarLeading) { Button("Cancel") { showRecovery = false } } }
                }
            }
        }
        .onAppear { model.refreshVault() }
    }

    private func run(_ operation: @escaping () async throws -> Void) {
        error = nil
        working = true
        Task {
            do { try await operation() }
            catch { self.error = "Couldn’t connect. Check your connection and try again. You can find more information in Connection details." }
            working = false
        }
    }
}
