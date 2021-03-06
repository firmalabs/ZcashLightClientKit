//
//  ZcashSDK+extensions.swift
//  ZcashLightClientKit
//
//  Created by Francisco Gindre on 12/8/20.
//

import Foundation

/**
 Ideally this extension shouldn't exist. Fees should be calculated from inputs and outputs. "Perfect is the enemy of good"
 */
public extension ZcashSDK {
    
    /**
     Returns the default fee at the time of that blockheight.
     */

    static func defaultFee(for height: BlockHeight = BlockHeight.max) -> Int64 {
        guard  height >= feeChangeHeight else { return 10_000 }
        
        return 1_000
    }
    /**
     Estimated height where wallets are supposed to change the fee
     */
    private static var feeChangeHeight: BlockHeight {
        ZcashSDK.isMainnet ? 1_077_550 : 1_028_500
    }
}
