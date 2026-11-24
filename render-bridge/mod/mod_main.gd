extends Node

var native = null
var interface = null
var owns_viewport = false
var original_size = Vector2.ZERO
var stream_size = Vector2.ZERO
var previous_flip = false
var active_request = 0

func _init():
	pause_mode = Node.PAUSE_MODE_PROCESS

func _ready():
	if OS.get_name() != "Windows" or Engine.editor_hint:
		set_process(false)
		return
	var path = OS.get_environment("LAN_STREAM_GODOT_DLL")
	if path.empty():
		path = OS.get_executable_path().get_base_dir().plus_file("LanStreamRenderer.dll")
	if not File.new().file_exists(path):
		set_process(false)
		return
	var config = ConfigFile.new()
	config.set_value("entry", "Windows.64", path)
	config.set_value("general", "load_once", true)
	config.set_value("general", "reloadable", false)
	var library = GDNativeLibrary.new()
	library.config_file = config
	native = GDNative.new()
	native.library = library
	if not native.initialize():
		native = null
		set_process(false)
		return
	interface = ARVRServer.find_interface("LanStreamMono")
	set_process(interface != null)

func _process(_delta):
	if not native or not interface:
		return
	var requested = native.call_native("standard_varcall", "lan_stream_requested", [])
	var viewport = get_tree().get_root()
	if requested and not owns_viewport:
		if viewport.arvr or ARVRServer.primary_interface:
			return
		if interface.initialize():
			original_size = viewport.size
			stream_size = native.call_native("standard_varcall", "lan_stream_target_size", [])
			if stream_size.x >= 64 and stream_size.y >= 64:
				viewport.size = stream_size
			previous_flip = viewport.render_target_v_flip
			viewport.render_target_v_flip = not previous_flip
			viewport.arvr = true
			owns_viewport = true
			active_request = requested
	elif requested and owns_viewport and requested != active_request:
		var next_size = native.call_native("standard_varcall", "lan_stream_target_size", [])
		if next_size.x >= 64 and next_size.y >= 64:
			stream_size = next_size
			viewport.size = stream_size
		active_request = requested
	elif not requested and owns_viewport:
		_stop_viewport()

func _stop_viewport():
	if owns_viewport:
		var viewport = get_tree().get_root()
		viewport.arvr = false
		viewport.render_target_v_flip = previous_flip
		if viewport.size == stream_size:
			viewport.size = original_size
		VisualServer.force_sync()
		interface.uninitialize()
		owns_viewport = false
		active_request = 0

func _exit_tree():
	_stop_viewport()
	if interface:
		ARVRServer.remove_interface(interface)
		interface = null
	if native:
		native.terminate()
		native = null
