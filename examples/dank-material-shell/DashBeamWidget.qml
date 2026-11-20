import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import QtQuick.Dialogs
import Quickshell
import Quickshell.Io
import qs.Common
import qs.Widgets
import qs.Modules.Plugins

PluginComponent {
    id: root
    component ActionButton: Button {
        id: control
        implicitHeight: 36
        implicitWidth: Math.max(80, label.implicitWidth + 24)
        leftPadding: 12
        rightPadding: 12
        topPadding: 6
        bottomPadding: 6
        Layout.minimumWidth: Math.min(implicitWidth, root.popoutWidth - 32)
        contentItem: Text {
            id: label
            text: control.text
            font.pixelSize: 13
            font.weight: control.highlighted ? Font.DemiBold : Font.Normal
            color: control.highlighted && control.enabled ? Theme.onPrimary : Theme.surfaceText
            opacity: control.enabled ? 1 : 0.6
            horizontalAlignment: Text.AlignHCenter
            verticalAlignment: Text.AlignVCenter
            elide: Text.ElideRight
        }
        background: Rectangle {
            radius: 10
            color: control.highlighted && control.enabled ? Theme.primary : Theme.surfaceContainerHigh
            opacity: control.enabled ? 1 : 0.5
            border.width: control.hovered ? 1 : 0
            border.color: Theme.primary
        }
    }
    component CodeField: TextField {
        id: field
        implicitHeight: 38
        font.pixelSize: 13
        color: Theme.surfaceText
        placeholderTextColor: Theme.surfaceVariantText
        selectionColor: Theme.primary
        selectedTextColor: Theme.onPrimary
        leftPadding: 10
        rightPadding: 10
        background: Rectangle {
            radius: 8
            color: Theme.surfaceContainerHigh
            border.width: 1
            border.color: field.activeFocus ? Theme.primary : Theme.outlineVariant
        }
    }
    property bool showPairing: false
    property var snapshot: ({
            paired: [],
            nearby: [],
            events: {},
            history: []
        })
    property bool connected: false
    property string message: "Connecting to DashBeam…"
    property string selectedId: ""
    property string selectedName: "Shareable ticket"
    property bool selectedNearby: false
    property var paths: []
    property string lastTicket: ""
    property bool panelOpen: false
    property bool sending: false
    property string outputFolder: Quickshell.env("HOME") + "/Downloads"
    readonly property string client: Quickshell.env("HOME") + "/.local/bin/dashbeam-dms-client"
    readonly property int onlineCount: (snapshot.paired || []).filter(d => d.online).length + (snapshot.nearby || []).length
    readonly property var incoming: snapshot.events ? snapshot.events["paired-invite-received"] : null
    property var telemetry: null

    function request(req) {
        if (actionProcess.running) {
            message = "Wait for the current operation to finish";
            return;
        }
        actionProcess.actionName = req.action;
        sending = req.action === "send";
        actionProcess.command = [client, JSON.stringify(req)];
        actionProcess.running = true;
    }
    function copy(text) {
        request({
            action: "copy",
            text: text
        });
    }
    function refresh() {
        if (!statusProcess.running)
            statusProcess.running = true;
    }
    function localPath(url) {
        return decodeURIComponent(String(url).replace(/^file:\/\//, ""));
    }
    function formatBytes(n) {
        if (!n || n < 1024)
            return Math.round(n || 0) + " B";
        const units = ["KiB", "MiB", "GiB", "TiB"];
        let i = -1;
        do {
            n /= 1024;
            i++;
        } while (n >= 1024 && i < 3)
        return n.toFixed(1) + " " + units[i];
    }
    function updateTelemetry() {
        const ev = snapshot.events || {};
        let raw = ev["receive-progress"] || ev["transfer-progress"];
        if (typeof raw === "string") {
            const a = raw.split(":").map(Number);
            telemetry = a.length === 3 ? {
                done: a[0],
                total: a[1],
                speed: a[2] / 1000
            } : null;
        }
    }
    Timer {
        interval: root.panelOpen ? 1500 : 15000
        running: true
        repeat: true
        triggeredOnStart: true
        onTriggered: root.refresh()
    }
    Process {
        id: statusProcess
        command: [root.client, '{"action":"status"}']
        stdout: StdioCollector {
            onStreamFinished: {
                try {
                    const r = JSON.parse(text);
                    const wasConnected = root.connected;
                    root.connected = r.ok;
                    if (r.ok && !wasConnected)
                        root.message = "Choose a device or share a ticket";
                    if (r.ok) {
                        root.snapshot = r.data;
                        root.updateTelemetry();
                    } else if (!root.sending)
                        root.message = "DashBeam unavailable: start the background service";
                } catch (e) {
                    root.connected = false;
                }
            }
        }
    }
    Process {
        id: actionProcess
        property string actionName: ""
        stdout: StdioCollector {
            onStreamFinished: {
                root.sending = false;
                try {
                    const r = JSON.parse(text);
                    if (!r.ok)
                        root.message = r.error;
                    else if (actionProcess.actionName === "send") {
                        root.lastTicket = r.data.ticket || "";
                        root.message = r.data.delivered === false ? "Device unreachable. You can share the ticket instead." : (root.selectedId ? "Invite sent to " + root.selectedName : "Ready: copy the ticket to share");
                    } else if (actionProcess.actionName === "pairing") {
                        root.lastTicket = String(r.data);
                        root.message = "Pairing code valid for 2 minutes";
                    } else if (actionProcess.actionName === "copy")
                        root.message = "Copied";
                    else if (actionProcess.actionName === "stop") {
                        root.message = "Stopped";
                        root.lastTicket = "";
                        root.telemetry = null;
                    } else
                        root.message = "Started";
                } catch (e) {
                    root.message = "Invalid response from DashBeam";
                }
                root.refresh();
            }
        }
    }
    IpcHandler {
        target: "dashbeamDms"
        function toggle(): string {
            root.triggerPopout();
            return "ok";
        }
        function status(): string {
            return JSON.stringify({
                connected: root.connected,
                online: root.onlineCount,
                message: root.message,
                panelOpen: root.panelOpen,
                nearby: (root.snapshot.nearby || []).length,
                paired: (root.snapshot.paired || []).length
            });
        }
    }
    horizontalBarPill: Row {
        spacing: Theme.spacingXS
        DankIcon {
            name: "send"
            size: root.iconSize
            color: root.connected ? Theme.primary : Theme.surfaceVariantText
            anchors.verticalCenter: parent.verticalCenter
        }
        StyledText {
            text: root.onlineCount
            color: Theme.surfaceText
            anchors.verticalCenter: parent.verticalCenter
        }
    }
    verticalBarPill: DankIcon {
        name: "send"
        size: root.iconSize
        color: root.connected ? Theme.primary : Theme.surfaceVariantText
    }
    readonly property real maxPanelHeight: Math.max(260, (root.parentScreen ? root.parentScreen.height : 900) - root.barThickness - 80)
    popoutWidth: Math.min(440, root.parentScreen ? root.parentScreen.width - 40 : 440)
    popoutHeight: 0
    popoutContent: Component {
        Item {
            id: panel
            property var parentPopout: null
            Connections {
                target: parentPopout
                function onShouldBeVisibleChanged() {
                    root.panelOpen = parentPopout.shouldBeVisible;
                    if (root.panelOpen)
                        root.refresh();
                }
            }
            Component.onDestruction: root.panelOpen = false
            implicitWidth: root.popoutWidth
            implicitHeight: Math.min(contentColumn.implicitHeight + 32, root.maxPanelHeight)
            FileDialog {
                id: files
                title: "Choose files to send"
                fileMode: FileDialog.OpenFiles
                onAccepted: {
                    root.paths = selectedFiles.map(u => root.localPath(u));
                    root.message = root.paths.length + " files selected";
                }
            }
            FolderDialog {
                id: folders
                title: "Choose a folder to send"
                onAccepted: {
                    root.paths = [root.localPath(selectedFolder)];
                    root.message = "Folder selected";
                }
            }
            FolderDialog {
                id: destination
                title: "Choose a download folder"
                onAccepted: root.outputFolder = root.localPath(selectedFolder)
            }
            ScrollView {
                id: scroller
                anchors.fill: parent
                anchors.margins: 16
                clip: true
                contentWidth: availableWidth
                contentHeight: contentColumn.implicitHeight
                ScrollBar.vertical.policy: contentHeight > availableHeight ? ScrollBar.AlwaysOn : ScrollBar.AlwaysOff
                ColumnLayout {
                    id: contentColumn
                    height: implicitHeight
                    width: scroller.availableWidth
                    spacing: 8
                    RowLayout {
                        Layout.fillWidth: true
                        StyledText {
                            text: "DashBeam"
                            font.pixelSize: 22
                            font.bold: true
                            Layout.fillWidth: true
                        }
                        StyledText {
                            text: root.connected ? root.onlineCount + " online" : "Disconnected"
                            color: root.connected ? Theme.primary : Theme.surfaceVariantText
                        }
                        ActionButton {
                            text: "↻"
                            onClicked: root.refresh()
                        }
                    }
                    ActionButton {
                        visible: !root.connected
                        text: "Start DashBeam in the background"
                        onClicked: root.request({
                            action: "start"
                        })
                    }
                    StyledText {
                        text: root.message
                        wrapMode: Text.Wrap
                        Layout.fillWidth: true
                        color: Theme.surfaceVariantText
                        font.pixelSize: 13
                    }
                    StyledText {
                        text: "DESTINATION"
                        font.bold: true
                        font.pixelSize: 12
                        color: Theme.primary
                    }
                    ActionButton {
                        text: "Shareable ticket"
                        highlighted: !root.selectedId
                        onClicked: {
                            root.selectedId = "";
                            root.selectedName = "Shareable ticket";
                        }
                        Layout.fillWidth: true
                    }
                    StyledText {
                        text: "Paired devices"
                        font.bold: true
                    }
                    Repeater {
                        model: root.snapshot.paired || []
                        delegate: ActionButton {
                            required property var modelData
                            Layout.fillWidth: true
                            text: (modelData.online ? "● " : "○ ") + modelData.display_name + (modelData.online ? "" : " · offline")
                            enabled: modelData.online
                            highlighted: root.selectedId === modelData.endpoint_id
                            onClicked: {
                                root.selectedId = modelData.endpoint_id;
                                root.selectedName = modelData.display_name;
                                root.selectedNearby = false;
                            }
                        }
                    }
                    StyledText {
                        visible: !(root.snapshot.paired || []).length
                        text: "No paired devices"
                        color: Theme.surfaceVariantText
                        font.pixelSize: 13
                    }
                    StyledText {
                        text: "Nearby on your local network"
                        font.bold: true
                    }
                    Repeater {
                        model: root.snapshot.nearby || []
                        delegate: ActionButton {
                            required property var modelData
                            Layout.fillWidth: true
                            text: "◎ " + (modelData.display_name || modelData.endpoint_id.slice(0, 12))
                            highlighted: root.selectedId === modelData.endpoint_id
                            onClicked: {
                                root.selectedId = modelData.endpoint_id;
                                root.selectedName = modelData.display_name || "Nearby device";
                                root.selectedNearby = true;
                            }
                        }
                    }
                    StyledText {
                        visible: !(root.snapshot.nearby || []).length
                        text: "No nearby devices. Run DashBeam and enable discoverability on the other device."
                        wrapMode: Text.Wrap
                        Layout.fillWidth: true
                        color: Theme.surfaceVariantText
                        font.pixelSize: 13
                    }
                    StyledText {
                        text: "SEND TO " + root.selectedName
                        font.bold: true
                        font.pixelSize: 12
                        color: Theme.primary
                        Layout.fillWidth: true
                        elide: Text.ElideRight
                    }
                    RowLayout {
                        ActionButton {
                            text: "File…"
                            onClicked: files.open()
                            enabled: root.connected
                        }
                        ActionButton {
                            text: "Folder…"
                            onClicked: folders.open()
                            enabled: root.connected
                        }
                        ActionButton {
                            text: root.sending ? "Preparing…" : "Send"
                            enabled: root.connected && root.paths.length > 0 && !root.sending
                            highlighted: true
                            onClicked: root.request({
                                action: "send",
                                paths: root.paths,
                                endpoint_id: root.selectedId,
                                nearby: root.selectedNearby
                            })
                        }
                    }
                    StyledText {
                        visible: root.paths.length > 0
                        text: root.paths.map(p => p.split("/").pop()).join(", ")
                        wrapMode: Text.Wrap
                        Layout.fillWidth: true
                        font.pixelSize: 12
                        color: Theme.surfaceVariantText
                    }
                    ActionButton {
                        visible: !!root.lastTicket
                        text: "Copy ticket"
                        onClicked: root.copy(root.lastTicket)
                    }
                    ProgressBar {
                        visible: !!root.telemetry
                        Layout.fillWidth: true
                        from: 0
                        to: root.telemetry ? Math.max(root.telemetry.total, 1) : 1
                        value: root.telemetry ? root.telemetry.done : 0
                    }
                    StyledText {
                        visible: !!root.telemetry
                        text: root.telemetry ? root.formatBytes(root.telemetry.done) + " / " + root.formatBytes(root.telemetry.total) + " · " + root.formatBytes(root.telemetry.speed) + "/s" : ""
                        font.pixelSize: 12
                    }
                    StyledText {
                        Layout.fillWidth: true
                        wrapMode: Text.Wrap
                        font.pixelSize: 13
                        visible: text.length > 0
                        text: {
                            const ev = root.snapshot.events || {};
                            if (ev["receive-error"])
                                return "Receive failed: " + String(ev["receive-error"]);
                            if (ev["transfer-failed"])
                                return "Send failed";
                            if (ev["receive-completed"] || ev["transfer-completed"])
                                return "Transfer completed";
                            return "";
                        }
                    }
                    ActionButton {
                        visible: !!root.snapshot.ticket || root.snapshot.active
                        text: "Stop transfer / sharing"
                        onClicked: root.request({
                            action: "stop"
                        })
                    }
                    StyledText {
                        text: "RECEIVE"
                        font.bold: true
                        font.pixelSize: 12
                        color: Theme.primary
                    }
                    CodeField {
                        id: receiveCode
                        placeholderText: "Paste a transfer ticket"
                        Layout.fillWidth: true
                        selectByMouse: true
                    }
                    RowLayout {
                        ActionButton {
                            text: "Save to…"
                            onClicked: destination.open()
                        }
                        ActionButton {
                            text: "Receive"
                            enabled: root.connected && receiveCode.text.trim().length > 0
                            onClicked: root.request({
                                action: "receive",
                                ticket: receiveCode.text.trim(),
                                output: root.outputFolder
                            })
                        }
                    }
                    StyledText {
                        text: root.outputFolder
                        font.pixelSize: 12
                        elide: Text.ElideMiddle
                        Layout.fillWidth: true
                        color: Theme.surfaceVariantText
                    }
                    ColumnLayout {
                        visible: !!root.incoming
                        Layout.fillWidth: true
                        StyledText {
                            text: root.incoming ? "Invitation from " + root.incoming.sender_name : ""
                            font.bold: true
                        }
                        StyledText {
                            text: root.incoming ? root.incoming.file_count + " files · " + root.formatBytes(root.incoming.total_size) + " · ID " + root.incoming.remote_endpoint_id.slice(0, 12) : ""
                            font.pixelSize: 12
                        }
                        RowLayout {
                            ActionButton {
                                text: "Accept"
                                onClicked: root.request({
                                    action: "respond",
                                    accepted: true,
                                    endpoint_id: root.incoming.remote_endpoint_id,
                                    output: root.outputFolder
                                })
                            }
                            ActionButton {
                                text: "Decline"
                                onClicked: root.request({
                                    action: "respond",
                                    accepted: false,
                                    endpoint_id: root.incoming.remote_endpoint_id
                                })
                            }
                        }
                    }
                    ActionButton {
                        text: root.showPairing ? "Hide pairing options" : "Pair a device…"
                        onClicked: root.showPairing = !root.showPairing
                        Layout.fillWidth: true
                    }
                    RowLayout {
                        visible: root.showPairing
                        ActionButton {
                            text: "Generate code"
                            onClicked: root.request({
                                action: "pairing"
                            })
                            enabled: root.connected
                        }
                        ActionButton {
                            text: "Pair nearby"
                            enabled: root.connected && root.selectedNearby && !!root.selectedId
                            onClicked: root.request({
                                action: "pair_nearby",
                                endpoint_id: root.selectedId
                            })
                        }
                    }
                    CodeField {
                        id: pairCode
                        visible: root.showPairing
                        placeholderText: "Other device’s pairing code"
                        Layout.fillWidth: true
                        selectByMouse: true
                    }
                    ActionButton {
                        visible: root.showPairing
                        text: "Pair with code"
                        enabled: root.connected && pairCode.text.trim().length > 0
                        onClicked: root.request({
                            action: "join",
                            ticket: pairCode.text.trim()
                        })
                    }
                }
            }
        }
    }
}
