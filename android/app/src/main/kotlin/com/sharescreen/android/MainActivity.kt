package com.sharescreen.android

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.viewModels
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.Surface
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.Modifier
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import com.sharescreen.android.call.AppView
import com.sharescreen.android.call.CallViewModel
import com.sharescreen.android.ui.CallScreen
import com.sharescreen.android.ui.HomeScreen
import com.sharescreen.android.ui.theme.SharescreenTheme

class MainActivity : ComponentActivity() {

    private val viewModel: CallViewModel by viewModels()

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()

        setContent {
            SharescreenTheme {
                Surface(modifier = Modifier.fillMaxSize()) {
                    val uiState by viewModel.uiState.collectAsStateWithLifecycle()
                    when (uiState.view) {
                        AppView.Home -> HomeScreen(viewModel)
                        AppView.Call -> CallScreen(viewModel)
                    }
                }
            }
        }
    }
}
