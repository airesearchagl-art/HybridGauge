import { useState, useEffect, useCallback } from "react";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import {
  Thermometer, Monitor, Wind, Database, Activity, Zap, Shield, Layers
} from "lucide-react";
import "./App.css";

// ── Reusable primitives ────────────────────────────────────────────

function Bar({ value, max = 100, color }) {
  const pct = Math.min(100, Math.max(0, (value / max) * 100));
  return (
    <div className="bar-track">
      <div className="bar-fill" style={{ width: `${pct}%`, background: color }} />
    </div>
  );
}

function Metric({ icon: Icon, label, value, unit, bar, color }) {
  return (
    <div className="metric">
      <div className="metric-header">
        <Icon size={14} color={color} />
        <span className="metric-label">{label}</span>
        <span className="metric-value">
          {value !== null && value !== undefined ? `${value}${unit ?? ""}` : "N/A"}
        </span>
      </div>
      {bar && value !== null && value !== undefined && (
        <Bar value={value} max={bar} color={color} />
      )}
    </div>
  );
}

function Card({ title, accent, children }) {
  return (
    <div className="card" style={{ borderTopColor: accent }}>
      <h2 className="card-title" style={{ color: accent }}>{title}</h2>
      {children}
    </div>
  );
}

// ── Fan Speed Slider ───────────────────────────────────────────────
// override: null = auto mode, number = manual speed %

function FanSlider({ gpuIndex, currentFanSpeed, fanControlAvailable, safetyActive, override, onOverrideChange }) {
  const isManual = override !== null && override !== undefined;
  const sliderVal = isManual ? override : (currentFanSpeed ?? 50);

  const handleToggle = useCallback(async () => {
    if (!isManual) {
      const initial = currentFanSpeed ?? 50;
      await onOverrideChange(gpuIndex, initial);
    } else {
      await onOverrideChange(gpuIndex, null);
    }
  }, [isManual, gpuIndex, currentFanSpeed, onOverrideChange]);

  const handleSlider = useCallback(async (e) => {
    await onOverrideChange(gpuIndex, Number(e.target.value));
  }, [gpuIndex, onOverrideChange]);

  if (!fanControlAvailable) {
    return <p className="note">ファン手動制御: NVIDIAドライバー 520+ が必要です</p>;
  }

  return (
    <div className="fan-control">
      <div className="fan-control-header">
        <Wind size={14} color="#03a9f4" />
        <span className="metric-label">ファン手動制御</span>
        {safetyActive && (
          <span className="safety-badge">
            <Shield size={11} />
            安全制御中
          </span>
        )}
        <button
          className={`fan-toggle ${isManual && !safetyActive ? "active" : ""}`}
          onClick={handleToggle}
          disabled={safetyActive}
          title={safetyActive ? "温度85°C超えのため安全制御中" : ""}
        >
          {isManual && !safetyActive ? "手動" : "自動"}
        </button>
      </div>
      {isManual && !safetyActive && (
        <div className="fan-slider-row">
          <input
            type="range" min={0} max={100} value={sliderVal}
            onChange={handleSlider}
            className="fan-slider"
          />
          <span className="fan-slider-val">{sliderVal}%</span>
        </div>
      )}
    </div>
  );
}

// ── GPU Cards ──────────────────────────────────────────────────────

function NvidiaCard({ gpu, fanOverride, onOverrideChange }) {
  return (
    <Card title={`NVIDIA — ${gpu.name}`} accent="#76b900">
      <Metric icon={Activity}    label="GPU Load"   value={gpu.utilization_gpu} unit="%" bar={100} color="#76b900" />
      <Metric icon={Database}    label="VRAM Load"  value={gpu.utilization_mem} unit="%" bar={100} color="#4caf50" />
      <Metric
        icon={Thermometer}
        label="温度"
        value={gpu.temperature}
        unit="°C"
        bar={110}
        color={gpu.temperature >= 85 ? "#f44336" : "#ff9800"}
      />
      <Metric icon={Wind}        label="ファン速度" value={gpu.fan_speed} unit="%" bar={100} color="#03a9f4" />
      {gpu.vram_total_mb && (
        <Metric
          icon={Database}
          label="VRAM使用量"
          value={`${gpu.vram_used_mb} / ${gpu.vram_total_mb}`}
          unit=" MB"
          bar={gpu.vram_total_mb}
          color="#9c27b0"
        />
      )}
      <FanSlider
        gpuIndex={gpu.index}
        currentFanSpeed={gpu.fan_speed}
        fanControlAvailable={gpu.fan_control_available}
        safetyActive={gpu.safety_override_active}
        override={fanOverride}
        onOverrideChange={onOverrideChange}
      />
    </Card>
  );
}

function AmdCard({ gpu }) {
  return (
    <Card title={`AMD — ${gpu.name}`} accent="#ed1c24">
      <Metric icon={Activity}    label="GPU Load (3D)" value={gpu.utilization_3d != null ? Math.round(gpu.utilization_3d) : null} unit="%" bar={100} color="#ed1c24" />
      <Metric icon={Thermometer} label="温度"           value={gpu.temperature != null ? Math.round(gpu.temperature) : null}       unit="°C" bar={110} color="#ff9800" />
      {gpu.vram_mb && (
        <Metric icon={Database}  label="VRAM"          value={gpu.vram_mb} unit=" MB" color="#9c27b0" />
      )}
      {gpu.temperature === null && (
        <p className="note">温度取得不可: LibreHardwareMonitorのインストールを推奨</p>
      )}
    </Card>
  );
}

function CpuCard({ cpu }) {
  return (
    <Card title={`CPU — ${cpu.name}`} accent="#2196f3">
      <Metric icon={Zap}         label="全コア合計"     value={Math.round(cpu.overall_usage)} unit="%" bar={100} color="#2196f3" />
      <Metric icon={Thermometer} label="温度 (Package)" value={cpu.package_temp != null ? Math.round(cpu.package_temp) : null} unit="°C" bar={110} color="#ff9800" />
      <div className="core-grid">
        {cpu.core_usages.map((usage, i) => (
          <div key={i} className="core-cell">
            <span className="core-label">C{i}</span>
            <Bar value={usage} color="#2196f3" />
            <span className="core-value">{Math.round(usage)}%</span>
          </div>
        ))}
      </div>
    </Card>
  );
}

// ── Root App ───────────────────────────────────────────────────────

const RENDERING_PRESET_SPEED = 80;

export default function App() {
  const [metrics, setMetrics]       = useState(null);
  const [tick, setTick]             = useState(0);
  // fanOverrides: { [gpuIndex]: number | null }  null = auto
  const [fanOverrides, setFanOverrides] = useState({});
  const [presetActive, setPresetActive] = useState(false);

  useEffect(() => {
    let unlisten;
    listen("metrics-update", (event) => {
      setMetrics(event.payload);
      setTick((t) => t + 1);
    }).then((fn) => { unlisten = fn; });
    return () => { if (unlisten) unlisten(); };
  }, []);

  // Central fan-override handler used by sliders and preset button
  const handleOverrideChange = useCallback(async (index, speed) => {
    try {
      await invoke("set_fan_speed", { index, speed: speed ?? null });
      setFanOverrides(prev => ({ ...prev, [index]: speed ?? null }));
    } catch (e) {
      console.error("set_fan_speed failed:", e);
    }
  }, []);

  // Rendering preset: set all controllable NVIDIA GPUs to 80%
  const handleRenderingPreset = useCallback(async () => {
    if (!metrics) return;
    const targets = metrics.nvidia_gpus.filter(
      g => g.fan_control_available && !g.safety_override_active
    );
    await Promise.all(
      targets.map(g => handleOverrideChange(g.index, RENDERING_PRESET_SPEED))
    );
    setPresetActive(true);
  }, [metrics, handleOverrideChange]);

  // Detect when user manually moves a slider away from preset — clear preset indicator
  const handleOverrideChangeWithPresetReset = useCallback(async (index, speed) => {
    if (speed !== RENDERING_PRESET_SPEED) setPresetActive(false);
    await handleOverrideChange(index, speed);
  }, [handleOverrideChange]);

  const hasNvidiaFanControl = metrics?.nvidia_gpus.some(
    g => g.fan_control_available && !g.safety_override_active
  );

  return (
    <div className="app">
      <header className="header">
        <Monitor size={20} />
        <span className="app-title">HybridGauge</span>
        {hasNvidiaFanControl && (
          <button
            className={`preset-btn ${presetActive ? "active" : ""}`}
            onClick={handleRenderingPreset}
            title={`全NVIDIAファンを${RENDERING_PRESET_SPEED}%に固定`}
          >
            <Layers size={13} />
            レンダリング・プリセット
          </button>
        )}
        <span className="tick">#{tick}</span>
      </header>

      {!metrics ? (
        <div className="loading">センサー初期化中...</div>
      ) : (
        <div className="dashboard">
          {metrics.nvidia_gpus.map((gpu) => (
            <NvidiaCard
              key={gpu.index}
              gpu={gpu}
              fanOverride={fanOverrides[gpu.index] ?? null}
              onOverrideChange={handleOverrideChangeWithPresetReset}
            />
          ))}
          {metrics.amd_gpus.map((gpu, i) => (
            <AmdCard key={i} gpu={gpu} />
          ))}
          {metrics.nvidia_gpus.length === 0 && metrics.amd_gpus.length === 0 && (
            <p className="note" style={{ gridColumn: "1 / -1" }}>
              GPUが検出されませんでした（管理者権限で起動しているか確認してください）
            </p>
          )}
          <CpuCard cpu={metrics.cpu} />
        </div>
      )}
    </div>
  );
}
