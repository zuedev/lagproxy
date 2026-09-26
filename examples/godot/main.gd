extends Node2D
## Lobby-free host/join demo. The server listens on 7778 directly; clients
## connect to 7777, which is where lagproxy should be listening.

const PLAYER := preload("res://player.tscn")
const SERVER_PORT := 7778


func _ready() -> void:
	%HostButton.pressed.connect(_host)
	%JoinButton.pressed.connect(_join)
	$PingTimer.timeout.connect(_send_ping)
	multiplayer.peer_connected.connect(_on_peer_connected)
	multiplayer.peer_disconnected.connect(_on_peer_disconnected)
	multiplayer.connected_to_server.connect(
		func() -> void: _set_status("Connected as peer %d" % multiplayer.get_unique_id()))
	multiplayer.connection_failed.connect(func() -> void: _set_status("Connection failed"))
	multiplayer.server_disconnected.connect(func() -> void: _set_status("Server went away"))

	# `godot --path examples/godot -- --host` / `-- --join` skips the buttons.
	var args := OS.get_cmdline_user_args()
	if "--host" in args:
		_host()
	elif "--join" in args:
		_join()


func _host() -> void:
	var peer := ENetMultiplayerPeer.new()
	if peer.create_server(SERVER_PORT) != OK:
		_set_status("Cannot listen on :%d" % SERVER_PORT)
		return
	multiplayer.multiplayer_peer = peer
	_spawn_player(1)
	_set_status("Hosting on :%d" % SERVER_PORT)
	_lock_buttons()


func _join() -> void:
	var parts: PackedStringArray = %Address.text.split(":")
	if parts.size() != 2:
		_set_status("Address must be host:port")
		return
	var peer := ENetMultiplayerPeer.new()
	if peer.create_client(parts[0], int(parts[1])) != OK:
		_set_status("Cannot connect to %s" % %Address.text)
		return
	multiplayer.multiplayer_peer = peer
	_set_status("Connecting to %s..." % %Address.text)
	_lock_buttons()


func _on_peer_connected(id: int) -> void:
	if multiplayer.is_server():
		_spawn_player(id)


func _on_peer_disconnected(id: int) -> void:
	if multiplayer.is_server() and $Players.has_node(str(id)):
		$Players.get_node(str(id)).queue_free()


## Server only. The MultiplayerSpawner replicates the new node to every client.
func _spawn_player(id: int) -> void:
	var player := PLAYER.instantiate()
	player.name = str(id)
	player.position = Vector2(400, 300) + Vector2(randf_range(-150, 150), randf_range(-100, 100))
	$Players.add_child(player)


func _send_ping() -> void:
	if multiplayer.multiplayer_peer and not multiplayer.is_server() \
			and multiplayer.multiplayer_peer.get_connection_status() == MultiplayerPeer.CONNECTION_CONNECTED:
		_ping.rpc_id(1, Time.get_ticks_msec())


@rpc("any_peer", "unreliable")
func _ping(sent_ms: int) -> void:
	_pong.rpc_id(multiplayer.get_remote_sender_id(), sent_ms)


@rpc("authority", "unreliable")
func _pong(sent_ms: int) -> void:
	%Ping.text = "RTT %d ms" % (Time.get_ticks_msec() - sent_ms)


func _set_status(text: String) -> void:
	%Status.text = text
	print(text)


func _lock_buttons() -> void:
	%HostButton.disabled = true
	%JoinButton.disabled = true
	%Address.editable = false
