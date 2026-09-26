extends Node2D
## Server-authoritative square. The owning client sends its input every physics
## tick and predicts locally; the server integrates the input and the
## MultiplayerSynchronizer pushes `position` back to everyone.
##
## With lagproxy in between, the gap between the outline (prediction) and the
## filled square (server truth) IS the latency, and lost input packets show up
## as the server square stalling and then rubber-banding.

const SPEED := 200.0
const SIZE := Vector2(32, 32)
## Beyond this the prediction is clearly wrong (lost packets), so snap to the server.
const SNAP_DISTANCE := 150.0

var peer_id := 1
var color := Color.WHITE
var _input := Vector2.ZERO # server side: last input received from the owner
var _predicted := Vector2.ZERO # owner side: where we think we are


func _ready() -> void:
	peer_id = name.to_int()
	color = Color.from_hsv(fmod(peer_id * 0.618, 1.0), 0.7, 1.0)
	_predicted = position


func _physics_process(delta: float) -> void:
	if multiplayer.is_server():
		position += _input * SPEED * delta

	if _is_mine():
		var dir := Input.get_vector("ui_left", "ui_right", "ui_up", "ui_down")
		if multiplayer.is_server():
			_input = dir
		else:
			_send_input.rpc_id(1, dir)
		_predicted += dir * SPEED * delta
		if dir == Vector2.ZERO or _predicted.distance_to(position) > SNAP_DISTANCE:
			_predicted = _predicted.lerp(position, 0.2) # converge once the server catches up
	queue_redraw()


@rpc("any_peer", "unreliable_ordered")
func _send_input(dir: Vector2) -> void:
	if multiplayer.get_remote_sender_id() == peer_id:
		_input = dir.limit_length(1.0)


func _draw() -> void:
	draw_rect(Rect2(-SIZE / 2, SIZE), color)
	draw_string(ThemeDB.fallback_font, Vector2(-8, SIZE.y), str(peer_id), HORIZONTAL_ALIGNMENT_LEFT, -1, 14)
	if _is_mine():
		var offset := _predicted - position
		draw_line(Vector2.ZERO, offset, color, 1.0)
		draw_rect(Rect2(offset - SIZE / 2, SIZE), color, false, 2.0)


func _is_mine() -> bool:
	return peer_id == multiplayer.get_unique_id()
