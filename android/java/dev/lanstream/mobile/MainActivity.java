package dev.lanstream.mobile;

import android.app.Activity;
import android.app.AlertDialog;
import android.content.SharedPreferences;
import android.content.res.ColorStateList;
import android.graphics.Matrix;
import android.graphics.Point;
import android.graphics.RectF;
import android.graphics.SurfaceTexture;
import android.graphics.Typeface;
import android.graphics.drawable.GradientDrawable;
import android.graphics.drawable.RippleDrawable;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.text.InputFilter;
import android.text.InputType;
import android.util.Log;
import android.view.Display;
import android.view.Gravity;
import android.view.Surface;
import android.view.SurfaceHolder;
import android.view.SurfaceView;
import android.view.TextureView;
import android.view.View;
import android.view.WindowInsets;
import android.view.WindowManager;
import android.widget.ArrayAdapter;
import android.widget.Button;
import android.widget.EditText;
import android.widget.FrameLayout;
import android.widget.ImageView;
import android.widget.LinearLayout;
import android.widget.ProgressBar;
import android.widget.ScrollView;
import android.widget.SeekBar;
import android.widget.Spinner;
import android.widget.TextView;
import android.widget.Toast;
import java.util.LinkedHashMap;
import org.json.JSONObject;

public final class MainActivity extends Activity {
    private static final int BACKGROUND = 0xff111715, PANEL = 0xff1b2420, BORDER = 0xff3c4c42;
    private static final int TEXT = 0xfff0f5ed, MUTED = 0xffa6b6a9, ACCENT = 0xffdbfa72;

    private final Handler ui = new Handler(Looper.getMainLooper());
    private SharedPreferences preferences;
    private FrameLayout play;
    private LinearLayout editBar;
    private TextureView texture;
    private SurfaceView directView;
    private TouchOverlay overlay;
    private Button menu;
    private TextView connectionStatus;
    private Profile profile;
    private Stream stream;
    private MediaRunner media;
    private long session;
    private int generation;
    private int insetLeft, insetTop, insetRight, insetBottom;
    private boolean editing, muted, foreground, connecting;
    private boolean controlsVisible = true;
    private boolean preferUdp = true;
    private long lastUdpRetry;
    private int displayModeBefore;
    private float refreshBefore;
    private boolean displayModeRequested;
    private float sourceInsetX, sourceInsetY;
    private String lastPlacement;
    private final Runnable autoRetry = this::connect;

    private final Runnable heartbeat = new Runnable() {
        @Override public void run() {
            if (overlay == null || session == 0) return;
            overlay.heartbeat();
            ui.postDelayed(this, 350);
        }
    };

    @Override public void onCreate(Bundle saved) {
        super.onCreate(saved);
        preferences = getSharedPreferences("lan_stream", MODE_PRIVATE);
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        if (Build.VERSION.SDK_INT >= 28) {
            WindowManager.LayoutParams window = getWindow().getAttributes();
            window.layoutInDisplayCutoutMode = WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_SHORT_EDGES;
            getWindow().setAttributes(window);
        }
        immersive();
        showWaiting("正在查找电脑主机…");
    }

    @Override protected void onResume() {
        super.onResume();
        foreground = true;
        preferUdp = true;
        lastUdpRetry = 0;
        immersive();
        if (session == 0) {
            showWaiting("正在查找电脑主机…");
            connect();
        }
    }

    @Override protected void onPause() {
        foreground = false;
        stopPlay();
        super.onPause();
    }

    @Override public void onWindowFocusChanged(boolean focus) {
        super.onWindowFocusChanged(focus);
        if (focus) immersive();
        else if (overlay != null) overlay.reset();
    }

    @Override public void onBackPressed() {
        if (editing) edit(false);
        else if (session != 0) showMenu();
        else super.onBackPressed();
    }

    private void immersive() {
        getWindow().getDecorView().setSystemUiVisibility(View.SYSTEM_UI_FLAG_FULLSCREEN | View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
            | View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY | View.SYSTEM_UI_FLAG_LAYOUT_STABLE
            | View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN | View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION);
    }

    private void showWaiting(String message) {
        FrameLayout page = new FrameLayout(this);
        page.setBackgroundColor(BACKGROUND);
        LinearLayout card = column();
        card.setPadding(dp(24), dp(22), dp(24), dp(22));
        card.setBackground(shape(PANEL, 18, BORDER));
        page.addView(card, new FrameLayout.LayoutParams(Math.min(dp(430), realSize().x - dp(44)), -2, Gravity.CENTER));

        LinearLayout heading = new LinearLayout(this);
        heading.setGravity(Gravity.CENTER_VERTICAL);
        card.addView(heading);
        ImageView icon = new ImageView(this);
        icon.setImageResource(R.drawable.ic_lan_stream);
        heading.addView(icon, new LinearLayout.LayoutParams(dp(52), dp(52)));
        TextView title = text(GameBinding.TITLE, 27, TEXT);
        title.setTypeface(Typeface.create("sans-serif-medium", Typeface.NORMAL));
        LinearLayout.LayoutParams titlePosition = new LinearLayout.LayoutParams(0, -2, 1);
        titlePosition.leftMargin = dp(13);
        heading.addView(title, titlePosition);
        ProgressBar indicator = new ProgressBar(this);
        indicator.setIndeterminateTintList(ColorStateList.valueOf(ACCENT));
        heading.addView(indicator, new LinearLayout.LayoutParams(dp(24), dp(24)));
        connectionStatus = text(message, 15, TEXT);
        addWithTop(card, connectionStatus, 13);
        addWithTop(card, text("电脑需运行串流主机，并与手机连接同一个局域网。", 13, MUTED), 8);
        setContentView(page);
        immersive();
    }

    private void connect() {
        if (!foreground || connecting || session != 0) return;
        connecting = true;
        int token = ++generation;
        Point display = realSize();
        Profile layout = Profile.load(preferences.getString("profile." + GameBinding.ID, null), GameBinding.XBOX);
        boolean phoneAspect = layout.mode.requestsPhone();
        new Thread(() -> {
            long connection = 0;
            try {
                connection = NativeBridge.open(GameBinding.HOST_HINT, GameBinding.PORT, GameBinding.ID, GameBinding.KEY,
                    phoneAspect ? display.x : 0, phoneAspect ? display.y : 0, preferUdp);
                Stream info = new Stream(NativeBridge.info(connection));
                long acquired = connection;
                ui.post(() -> {
                    if (token != generation || !foreground) { NativeBridge.close(acquired); return; }
                    connecting = false;
                    openPlay(acquired, info, layout, token);
                });
            } catch (Exception error) {
                if (connection != 0) NativeBridge.close(connection);
                ui.post(() -> {
                    if (token != generation || !foreground) return;
                    connecting = false;
                    if (connectionStatus != null) connectionStatus.setText(error.getMessage() == null ? "等待电脑主机…" : error.getMessage());
                    ui.postDelayed(autoRetry, 1800);
                });
            }
        }, "lan-connect").start();
    }

    private void openPlay(long handle, Stream info, Profile layout, int token) {
        session = handle;
        stream = info;
        profile = layout;
        editing = false;
        controlsVisible = true;
        sourceInsetX = sourceInsetY = 0;
        lastPlacement = null;
        connectionStatus = null;
        insetLeft = insetTop = insetRight = insetBottom = 0;
        play = new FrameLayout(this);
        play.setBackgroundColor(0xff080b0a);
        attachVideo(handle, info, token);

        overlay = new TouchOverlay(this, profile, new TouchOverlay.Callbacks() {
            @Override public void keys(int[] keys) { NativeBridge.keys(handle, keys); }
            @Override public void gamepad(int buttons, int x, int y) { NativeBridge.gamepad(handle, buttons, x, y); }
            @Override public void pointer(int action, int x, int y) { NativeBridge.pointer(handle, action, x, y); }
            @Override public void edit(Profile.Control control) { editControl(control); }
            @Override public void save() { saveProfile(); }
        });
        play.addView(overlay, new FrameLayout.LayoutParams(-1, -1));
        menu = button("⋯", false);
        menu.setTextSize(22);
        menu.setContentDescription("串流菜单");
        FrameLayout.LayoutParams menuPosition = new FrameLayout.LayoutParams(dp(50), dp(44), Gravity.TOP | Gravity.RIGHT);
        menuPosition.setMargins(dp(8), dp(8), dp(8), 0);
        play.addView(menu, menuPosition);
        menu.setOnClickListener(view -> showMenu());

        editBar = new LinearLayout(this);
        editBar.setGravity(Gravity.CENTER_VERTICAL);
        editBar.setPadding(dp(12), dp(4), dp(5), dp(4));
        editBar.setBackground(shape(PANEL, 12, BORDER));
        TextView editorHelp = text("拖动位置 · 点按改键", 13, TEXT);
        editBar.addView(editorHelp);
        Button add = button("＋ 按键", false);
        Button done = button("完成", true);
        addBeside(editBar, add);
        addBeside(editBar, done);
        add.setOnClickListener(view -> addControl());
        done.setOnClickListener(view -> edit(false));
        FrameLayout.LayoutParams barPosition = new FrameLayout.LayoutParams(-2, -2, Gravity.TOP | Gravity.CENTER_HORIZONTAL);
        barPosition.topMargin = dp(8);
        play.addView(editBar, barPosition);
        editBar.setVisibility(View.GONE);
        play.addOnLayoutChangeListener((view, left, top, right, bottom, a, b, c, d) -> {
            if (right-left != c-a || bottom-top != d-b) updateGeometry();
        });
        play.setOnApplyWindowInsetsListener((view, insets) -> {
            readCutout(insets);
            updateGeometry();
            return insets;
        });
        setContentView(play);
        play.requestApplyInsets();
        immersive();
        heartbeat.run();
        Log.i("LanStream", "Connected " + info.game + " " + info.width + "x" + info.height + " " + info.encoder + " profile=" + layout.mode + " transport=" + (info.udpVideo ? "UDP/FEC" : "TCP") + " fps=" + info.fps + " immediate=" + info.lowLatency);
        if (profile.mode == Profile.Mode.PHONE && !info.phoneAspect) {
            Toast.makeText(this, "游戏窗口保留原画幅，已完整显示。", Toast.LENGTH_LONG).show();
        }
    }

    private void attachVideo(long handle, Stream info, int token) {
        if (info.lowLatency) {
            preferDisplayMode(info.fps);
            directView = new SurfaceView(this);
            directView.setImportantForAccessibility(View.IMPORTANT_FOR_ACCESSIBILITY_NO);
            SurfaceHolder target = directView.getHolder();
            target.setFixedSize(info.width, info.height);
            target.addCallback(new SurfaceHolder.Callback() {
                @Override public void surfaceCreated(SurfaceHolder holder) {}
                @Override public void surfaceChanged(SurfaceHolder holder, int format, int width, int height) {
                    if (generation == token && media == null && holder.getSurface().isValid()) {
                        startMedia(handle, info, token, holder.getSurface(), false);
                    }
                }
                @Override public void surfaceDestroyed(SurfaceHolder holder) { surfaceLost(token); }
            });
            play.addView(directView, new FrameLayout.LayoutParams(-1, -1));
        } else {
            texture = new TextureView(this);
            texture.setSurfaceTextureListener(new TextureView.SurfaceTextureListener() {
                @Override public void onSurfaceTextureAvailable(SurfaceTexture surface, int width, int height) {
                    if (generation != token || media != null) return;
                    surface.setDefaultBufferSize(info.width, info.height);
                    startMedia(handle, info, token, new Surface(surface), true);
                }
                @Override public void onSurfaceTextureSizeChanged(SurfaceTexture surface, int width, int height) { updateGeometry(); }
                @Override public boolean onSurfaceTextureDestroyed(SurfaceTexture surface) { surfaceLost(token); return true; }
                @Override public void onSurfaceTextureUpdated(SurfaceTexture surface) {
                    if (generation == token && media != null) media.displayedFrame(System.nanoTime());
                }
            });
            play.addView(texture, new FrameLayout.LayoutParams(-1, -1));
        }
    }

    private void startMedia(long handle, Stream info, int token, Surface destination, boolean owned) {
        media = new MediaRunner(handle, destination, info.width, info.height, info.fps, muted, info.udpVideo, info.lowLatency, owned,
            (reason, tcpFallback) -> ui.post(() -> {
                if (generation != token) return;
                if (tcpFallback) {
                    long now = System.nanoTime();
                    preferUdp = lastUdpRetry == 0 || now - lastUdpRetry > 30_000_000_000L;
                    if (preferUdp) lastUdpRetry = now;
                }
                reconnect(tcpFallback && preferUdp ? "正在恢复低延迟视频连接…" : reason);
            }),
            (x, y) -> ui.post(() -> {
                if (generation != token) return;
                sourceInsetX = x;
                sourceInsetY = y;
                updateGeometry();
            }));
        Log.i("LanStream", "Output surface=" + (owned ? "TextureView" : "SurfaceView"));
        media.start();
        updateGeometry();
    }

    private void surfaceLost(int token) {
        if (generation == token) ui.post(() -> { if (generation == token) reconnect("画面已暂停，正在重新连接…"); });
    }

    private void preferDisplayMode(int fps) {
        Display display = getWindowManager().getDefaultDisplay();
        Display.Mode current = display.getMode();
        for (Display.Mode mode : display.getSupportedModes()) {
            if (mode.getPhysicalWidth() != current.getPhysicalWidth() || mode.getPhysicalHeight() != current.getPhysicalHeight()
                || Math.abs(mode.getRefreshRate() - fps) > 1f) continue;
            WindowManager.LayoutParams window = getWindow().getAttributes();
            displayModeBefore = window.preferredDisplayModeId;
            refreshBefore = window.preferredRefreshRate;
            displayModeRequested = true;
            window.preferredDisplayModeId = mode.getModeId();
            window.preferredRefreshRate = mode.getRefreshRate();
            getWindow().setAttributes(window);
            Log.i("LanStream", "Requested app refresh " + mode.getRefreshRate());
            break;
        }
    }

    private void readCutout(WindowInsets insets) {
        if (Build.VERSION.SDK_INT >= 28 && insets.getDisplayCutout() != null) {
            insetLeft = insets.getDisplayCutout().getSafeInsetLeft();
            insetTop = insets.getDisplayCutout().getSafeInsetTop();
            insetRight = insets.getDisplayCutout().getSafeInsetRight();
            insetBottom = insets.getDisplayCutout().getSafeInsetBottom();
        } else {
            insetLeft = insetTop = insetRight = insetBottom = 0;
        }
    }

    private void updateGeometry() {
        if (play == null || play.getWidth() == 0 || play.getHeight() == 0) return;
        int width = play.getWidth(), height = play.getHeight();
        RectF safe = new RectF(insetLeft + dp(9), insetTop + dp(6), width - insetRight - dp(9), height - insetBottom - dp(6));
        float rail = profile.mode == Profile.Mode.RAILS ? Math.min(dp(144), width * .18f) : 0;
        boolean cover = profile.mode == Profile.Mode.FILL;
        boolean useContent = profile.mode == Profile.Mode.AUTO || cover;
        VideoPlacement.Strategy strategy = cover ? VideoPlacement.Strategy.FILL : VideoPlacement.Strategy.FIT;
        VideoPlacement placement = VideoPlacement.place(width, height, stream.width, stream.height,
            useContent ? sourceInsetX : 0, useContent ? sourceInsetY : 0, rail, strategy);
        if (directView != null) {
            FrameLayout.LayoutParams position = (FrameLayout.LayoutParams)directView.getLayoutParams();
            int drawWidth = Math.round(placement.width), drawHeight = Math.round(placement.height);
            int left = Math.round(placement.left), top = Math.round(placement.top);
            if (position.width != drawWidth || position.height != drawHeight || position.leftMargin != left || position.topMargin != top) {
                position.width = drawWidth; position.height = drawHeight;
                position.leftMargin = left; position.topMargin = top;
                position.gravity = Gravity.TOP | Gravity.LEFT;
                directView.setLayoutParams(position);
            }
        } else if (texture != null) {
            Matrix transform = new Matrix();
            transform.setScale(placement.width / width, placement.height / height);
            transform.postTranslate(placement.left, placement.top);
            texture.setTransform(transform);
        }
        RectF full = new RectF(placement.left, placement.top, placement.left + placement.width, placement.top + placement.height);
        RectF picture = new RectF(placement.pictureLeft, placement.pictureTop,
            placement.pictureLeft + placement.pictureWidth, placement.pictureTop + placement.pictureHeight);
        overlay.geometry(safe, full, picture);
        String dimensions = profile.mode.name() + " picture=" + Math.round(placement.pictureWidth) + "x" + Math.round(placement.pictureHeight)
            + " screen=" + width + "x" + height + " bar=" + Math.round(sourceInsetX * stream.width) + "," + Math.round(sourceInsetY * stream.height);
        if (!dimensions.equals(lastPlacement)) { lastPlacement = dimensions; Log.i("LanStream", "Placement " + dimensions); }
        FrameLayout.LayoutParams position = (FrameLayout.LayoutParams)menu.getLayoutParams();
        int top = insetTop + dp(8), right = insetRight + dp(8);
        if (position.topMargin != top || position.rightMargin != right) {
            position.topMargin = top; position.rightMargin = right; menu.setLayoutParams(position);
        }
    }

    private void showMenu() {
        overlay.reset();
        String details = stream.width + " × " + stream.height + "  ·  " + profile.mode.label;
        String[] items = {"画面适配   " + details, "编辑触控布局", controlsVisible ? "隐藏触控键" : "显示触控键",
            profile.touchMouse ? "关闭点画面操作鼠标" : "开启点画面操作鼠标", muted ? "打开游戏声音" : "静音", "退出串流"};
        new AlertDialog.Builder(this).setTitle(stream.title).setItems(items, (dialog, index) -> {
            switch (index) {
                case 0: chooseMode(); break;
                case 1: edit(true); break;
                case 2: controlsVisible = !controlsVisible; overlay.setControlsVisible(controlsVisible); break;
                case 3: profile.touchMouse = !profile.touchMouse; saveProfile(); break;
                case 4: muted = !muted; if (media != null) media.mute(muted); break;
                case 5: stopPlay(); finish(); break;
                default: break;
            }
        }).setNegativeButton("返回游戏", null).show();
    }

    private void chooseMode() {
        String[] names = {"自动铺屏 · 自动调窗口，等比显示", "完整画面 · 保留原比例", "两侧控件 · 为手指留出侧栏", "铺满裁切 · 等比放大", "手机画幅 · 尝试调整主机窗口"};
        new AlertDialog.Builder(this).setTitle("画面适配").setSingleChoiceItems(names, profile.mode.ordinal(), (dialog, index) -> {
            dialog.dismiss();
            Profile.Mode before = profile.mode;
            Profile.Mode after = Profile.Mode.values()[index];
            if (before == after) return;
            profile.mode = after;
            saveProfile();
            if (before.requestsPhone() != after.requestsPhone()) {
                reconnect(after.requestsPhone() ? "正在调整游戏窗口…" : "正在恢复游戏画幅…");
            } else { updateGeometry(); }
        }).setNegativeButton("取消", null).show();
    }

    private void edit(boolean value) {
        editing = value;
        if (value && !controlsVisible) { controlsVisible = true; overlay.setControlsVisible(true); }
        overlay.setEditing(value);
        menu.setVisibility(value ? View.GONE : View.VISIBLE);
        editBar.setVisibility(value ? View.VISIBLE : View.GONE);
        if (!value) saveProfile();
    }

    private void addControl() {
        if (profile.buttons.size() >= Profile.MAX_BUTTONS) {
            Toast.makeText(this, "最多添加 " + Profile.MAX_BUTTONS + " 个按键", Toast.LENGTH_SHORT).show();
            return;
        }
        Profile.Control control = new Profile.Control(profile.xbox ? "X" : "空格", .72f, .53f, 25, profile.xbox ? 0x4000 : 32);
        profile.buttons.add(control);
        saveProfile();
        overlay.invalidate();
        editControl(control);
    }

    private void editControl(Profile.Control control) {
        LinearLayout form = column();
        form.setPadding(dp(22), dp(10), dp(22), dp(6));
        ScrollView scroll = new ScrollView(this);
        scroll.addView(form);
        boolean stick = control == profile.stick;
        Spinner[] keys = new Spinner[stick ? (profile.xbox ? 0 : 4) : 1];
        LinkedHashMap<Integer, String> bindings = profile.bindings();
        Integer[] codes = bindings.keySet().toArray(new Integer[0]);
        EditText label = stick ? null : field("屏幕上的名称", InputType.TYPE_CLASS_TEXT);
        if (label != null) {
            label.setText(control.label);
            label.setFilters(new InputFilter[]{ new InputFilter.LengthFilter(8) });
            form.addView(label);
        }
        LinearLayout rows = keys.length == 4 ? new LinearLayout(this) : form;
        if (keys.length == 4) {
            form.addView(rows, new LinearLayout.LayoutParams(-1, -2));
            rows.setGravity(Gravity.CENTER);
        }
        String[] directions = {"上", "左", "下", "右"};
        for (int i = 0; i < keys.length; i++) {
            LinearLayout item = column();
            if (stick) rows.addView(item, new LinearLayout.LayoutParams(0, -2, 1));
            else rows.addView(item);
            addWithTop(item, text(stick ? directions[i] : "发送到电脑的键", 13, MUTED), 7);
            Spinner selector = new Spinner(this, Spinner.MODE_DIALOG);
            ArrayAdapter<String> adapter = new ArrayAdapter<>(this, android.R.layout.simple_spinner_item, bindings.values().toArray(new String[0]));
            adapter.setDropDownViewResource(android.R.layout.simple_spinner_dropdown_item);
            selector.setAdapter(adapter);
            int selected = stick ? profile.directions[i] : control.key;
            for (int j = 0; j < codes.length; j++) if (codes[j] == selected) selector.setSelection(j);
            selector.setMinimumHeight(dp(41));
            item.addView(selector, new LinearLayout.LayoutParams(-1, -2));
            keys[i] = selector;
        }
        addWithTop(form, text("控件大小", 13, MUTED), 9);
        SeekBar size = new SeekBar(this);
        size.setMax(67);
        size.setProgress(Math.round(control.radius) - 18);
        form.addView(size);
        AlertDialog.Builder builder = new AlertDialog.Builder(this).setTitle(stick ? "摇杆设置" : "按键设置").setView(scroll)
            .setPositiveButton("保存", (dialog, index) -> {
                control.radius = 18 + size.getProgress();
                if (stick) {
                    for (int i = 0; i < keys.length; i++) profile.directions[i] = codes[keys[i].getSelectedItemPosition()];
                } else {
                    control.key = codes[keys[0].getSelectedItemPosition()];
                    String name = label.getText().toString().trim();
                    control.label = name.isEmpty() ? bindings.get(control.key).split(" · ")[0] : name;
                }
                saveProfile();
                overlay.invalidate();
            }).setNegativeButton("取消", null);
        if (!stick) builder.setNeutralButton("删除按键", (dialog, index) -> {
            profile.buttons.remove(control);
            saveProfile();
            overlay.invalidate();
        });
        AlertDialog dialog = builder.create();
        dialog.getWindow().setSoftInputMode(WindowManager.LayoutParams.SOFT_INPUT_STATE_ALWAYS_HIDDEN);
        dialog.show();
    }

    private void saveProfile() {
        if (profile != null) preferences.edit().putString("profile." + GameBinding.ID, profile.save()).apply();
    }

    private void reconnect(String reason) {
        stopPlay();
        showWaiting(reason);
        if (foreground) ui.postDelayed(autoRetry, 350);
    }

    private void stopPlay() {
        generation++;
        ui.removeCallbacks(heartbeat);
        ui.removeCallbacks(autoRetry);
        if (overlay != null) overlay.reset();
        if (media != null) media.stop(); else if (session != 0) NativeBridge.close(session);
        media = null; session = 0; play = null; texture = null; directView = null; overlay = null; stream = null; profile = null; editing = false; connecting = false;
        if (displayModeRequested) {
            displayModeRequested = false;
            WindowManager.LayoutParams window = getWindow().getAttributes();
            window.preferredDisplayModeId = displayModeBefore;
            window.preferredRefreshRate = refreshBefore;
            getWindow().setAttributes(window);
        }
    }

    private Point realSize() {
        Point size = new Point();
        getWindowManager().getDefaultDisplay().getRealSize(size);
        if (size.x < size.y) size.set(size.y, size.x);
        return size;
    }

    private int dp(float value) { return Math.round(value * getResources().getDisplayMetrics().density); }

    private LinearLayout column() { LinearLayout result = new LinearLayout(this); result.setOrientation(LinearLayout.VERTICAL); return result; }

    private void addWithTop(LinearLayout parent, View view, int top) {
        LinearLayout.LayoutParams layout = new LinearLayout.LayoutParams(-1, -2);
        layout.topMargin = dp(top);
        parent.addView(view, layout);
    }

    private void addBeside(LinearLayout parent, View view) {
        LinearLayout.LayoutParams layout = new LinearLayout.LayoutParams(-2, dp(43));
        layout.leftMargin = dp(7);
        parent.addView(view, layout);
    }

    private TextView text(String content, int size, int color) {
        TextView text = new TextView(this);
        text.setText(content); text.setTextSize(size); text.setTextColor(color);
        return text;
    }

    private EditText field(String hint, int type) {
        EditText field = new EditText(this);
        field.setSingleLine(true); field.setInputType(type); field.setHint(hint);
        field.setTypeface(Typeface.create("sans-serif", Typeface.NORMAL));
        field.setTextColor(TEXT); field.setHintTextColor(0xffa3b0a6); field.setTextSize(15);
        field.setPadding(dp(12), dp(7), dp(12), dp(7)); field.setMinHeight(dp(43));
        field.setBackground(shape(0xff141b17, 9, BORDER));
        return field;
    }

    private Button button(String title, boolean primary) {
        Button button = new Button(this);
        button.setText(title); button.setAllCaps(false); button.setTextSize(14);
        button.setTextColor(primary ? 0xff233016 : TEXT);
        button.setMinHeight(dp(46)); button.setMinimumHeight(dp(46)); button.setMinWidth(0); button.setMinimumWidth(0);
        button.setPadding(dp(13), 0, dp(13), 0); button.setStateListAnimator(null);
        GradientDrawable background = shape(primary ? ACCENT : 0xdc202a24, 10, primary ? ACCENT : BORDER);
        button.setBackground(new RippleDrawable(ColorStateList.valueOf(0x40a6bb9c), background, null));
        return button;
    }

    private GradientDrawable shape(int fill, int corner, int stroke) {
        GradientDrawable shape = new GradientDrawable();
        shape.setColor(fill); shape.setCornerRadius(dp(corner)); shape.setStroke(dp(1), stroke);
        return shape;
    }

    private static final class Stream {
        final String game, title, encoder;
        final int width, height, fps;
        final boolean phoneAspect;
        final boolean udpVideo;
        final boolean lowLatency;
        Stream(String metadata) throws Exception {
            JSONObject info = new JSONObject(metadata);
            game = info.getString("game_id"); title = info.getString("title"); encoder = info.getString("encoder");
            width = info.getInt("width"); height = info.getInt("height"); fps = info.getInt("fps");
            phoneAspect = info.getBoolean("phone_aspect");
            udpVideo = info.optBoolean("udp_video");
            lowLatency = info.optBoolean("low_latency");
            if (width < 64 || width > 3840 || height < 64 || height > 2160 || fps < 1 || fps > 120) throw new IllegalArgumentException("主机画面参数超出范围");
        }
    }
}
