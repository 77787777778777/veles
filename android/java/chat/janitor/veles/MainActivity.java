package chat.janitor.veles;

import android.Manifest;
import android.app.Activity;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.graphics.Typeface;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.util.TypedValue;
import android.view.View;
import android.view.WindowInsets;
import android.widget.Button;
import android.widget.LinearLayout;
import android.widget.ScrollView;
import android.widget.TextView;

/**
 * The gateway's state, Start and Stop, and the tail of the engine's
 * log. Built in code: four views do not need a layout file, and there
 * is nothing here to theme beyond the system's.
 */
public final class MainActivity extends Activity {
    private static final int LOG_TAIL = 32 << 10;

    private final Handler ui = new Handler(Looper.getMainLooper());
    private TextView status;
    private TextView log;
    private ScrollView logScroll;
    private Button start;
    private Button stop;
    private String shownLog = "";

    private final Runnable tick = new Runnable() {
        @Override
        public void run() {
            refresh();
            ui.postDelayed(this, 1000);
        }
    };

    @Override
    protected void onCreate(Bundle saved) {
        super.onCreate(saved);
        int pad = dp(16);

        LinearLayout root = new LinearLayout(this);
        root.setOrientation(LinearLayout.VERTICAL);
        root.setPadding(pad, pad, pad, pad);
        // Edge to edge is the only mode from Android 15 on: keep the
        // content out from under the status and navigation bars.
        root.setOnApplyWindowInsetsListener((v, insets) -> {
            int top, bottom;
            if (Build.VERSION.SDK_INT >= 30) {
                android.graphics.Insets bars = insets.getInsets(WindowInsets.Type.systemBars());
                top = bars.top;
                bottom = bars.bottom;
            } else {
                top = insets.getSystemWindowInsetTop();
                bottom = insets.getSystemWindowInsetBottom();
            }
            v.setPadding(pad, pad + top, pad, pad + bottom);
            return insets;
        });

        status = new TextView(this);
        status.setTextSize(TypedValue.COMPLEX_UNIT_SP, 20);
        root.addView(status);

        LinearLayout buttons = new LinearLayout(this);
        buttons.setOrientation(LinearLayout.HORIZONTAL);
        start = new Button(this);
        start.setText(R.string.action_start);
        start.setOnClickListener(v -> {
            GatewayService.start(this);
            refresh();
        });
        stop = new Button(this);
        stop.setText(R.string.action_stop);
        stop.setOnClickListener(v -> {
            GatewayService.stop(this);
            refresh();
        });
        buttons.addView(start);
        buttons.addView(stop);
        root.addView(buttons);

        TextView logTitle = new TextView(this);
        logTitle.setText(R.string.log_title);
        logTitle.setTypeface(Typeface.DEFAULT_BOLD);
        logTitle.setPadding(0, pad, 0, dp(4));
        root.addView(logTitle);

        log = new TextView(this);
        log.setTypeface(Typeface.MONOSPACE);
        log.setTextSize(TypedValue.COMPLEX_UNIT_SP, 11);
        log.setTextIsSelectable(true);
        logScroll = new ScrollView(this);
        logScroll.addView(log);
        root.addView(logScroll, new LinearLayout.LayoutParams(
                LinearLayout.LayoutParams.MATCH_PARENT, 0, 1));

        setContentView(root);

        // The service runs without it, but its notification (and the
        // Stop button on it) only shows with it, Android 13 on.
        if (Build.VERSION.SDK_INT >= 33
                && checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS)
                        != PackageManager.PERMISSION_GRANTED) {
            requestPermissions(new String[] {Manifest.permission.POST_NOTIFICATIONS}, 1);
        }
    }

    @Override
    protected void onResume() {
        super.onResume();
        // The engine stops on its own five minutes after anyone last
        // used it (GatewayService); coming back to the app starts it
        // again. On a fresh onCreate the service may still be coming up
        // from the start there, which a second start intent only
        // confirms.
        if (Prefs.enabled(this)) {
            startForegroundService(new Intent(this, GatewayService.class));
        }
        ui.post(tick);
    }

    @Override
    protected void onPause() {
        ui.removeCallbacks(tick);
        super.onPause();
    }

    private void refresh() {
        boolean on = Prefs.enabled(this);
        status.setText(GatewayService.describe(this));
        start.setEnabled(!on);
        stop.setEnabled(on);
        String text = Engine.tail(Engine.log(this), LOG_TAIL);
        if (text.isEmpty()) {
            text = getString(R.string.log_empty);
        }
        if (!text.equals(shownLog)) {
            // Follow the end only when the reader is already there.
            boolean atEnd = !logScroll.canScrollVertically(1);
            shownLog = text;
            log.setText(text);
            if (atEnd) {
                logScroll.post(() -> logScroll.fullScroll(View.FOCUS_DOWN));
            }
        }
    }

    private int dp(int v) {
        return Math.round(v * getResources().getDisplayMetrics().density);
    }
}
