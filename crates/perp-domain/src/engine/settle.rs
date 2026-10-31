//! Settlement of fills into positions, collateral and fee balances.

use super::Engine;
use crate::account::Position;
use crate::error::{Error, Result};
use crate::fixed::{ceil_div, notional_raw, Money, Price, Round};
use crate::types::{AccountId, MarketId, Side};

impl Engine {
    /// Apply one side of a trade to an account's position.
    ///
    /// `total` is the trade's notional, computed once by the caller and used
    /// for both counterparties. The part of the fill that reduces the position
    /// realises PnL against the money cost basis; the rest opens or extends it.
    ///
    /// For every account the change of `collateral - basis` is exactly
    /// `-side * total`, so the two counterparties cancel to zero.
    pub(super) fn apply_position(
        &mut self,
        account: AccountId,
        market: MarketId,
        side: Side,
        qty: u64,
        price: Price,
        total: Money,
    ) -> Result<()> {
        let sgn = i128::from(side.sign());
        let qty_signed = i64::try_from(qty).map_err(|_| Error::Overflow)?;
        let acct = self
            .accounts
            .get_mut(&account)
            .ok_or(Error::Internal("settle: no account"))?;
        let cur = acct.positions.get(&market).copied().unwrap_or_default();
        let cur_abs = cur.size.unsigned_abs();
        let closing = cur.size != 0 && cur.size.signum() != side.sign();
        let close_qty = if closing { qty.min(cur_abs) } else { 0 };
        let open_qty = qty - close_qty;

        let n_close = if close_qty == 0 {
            0
        } else if open_qty == 0 {
            total.0
        } else {
            notional_raw(price.0, close_qty, Round::Floor)?
                .0
                .min(total.0)
        };
        let n_open = total.0.checked_sub(n_close).ok_or(Error::Overflow)?;

        // Basis released rounds up (towards +inf): the closer realises less.
        let released = if close_qty == 0 {
            0
        } else if close_qty == cur_abs {
            cur.basis
        } else {
            ceil_div(
                cur.basis
                    .checked_mul(i128::from(close_qty))
                    .ok_or(Error::Overflow)?,
                i128::from(cur_abs),
            )?
        };
        let realized = if close_qty == 0 {
            0
        } else {
            sgn.checked_mul(n_close)
                .and_then(i128::checked_neg)
                .and_then(|cash| cash.checked_sub(released))
                .ok_or(Error::Overflow)?
        };
        let new_basis = cur
            .basis
            .checked_sub(released)
            .and_then(|b| b.checked_add(sgn.checked_mul(n_open)?))
            .ok_or(Error::Overflow)?;
        let new_size = cur
            .size
            .checked_add(side.sign().checked_mul(qty_signed).ok_or(Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        acct.collateral = acct
            .collateral
            .checked_add(realized)
            .ok_or(Error::Overflow)?;

        let flat = new_size == 0;
        if flat {
            if new_basis != 0 {
                return Err(Error::Internal("cost basis left on a flat position"));
            }
            acct.positions.remove(&market);
        } else {
            acct.positions.insert(
                market,
                Position {
                    size: new_size,
                    basis: new_basis,
                },
            );
        }
        let holders = &mut self.market_mut(market)?.holders;
        if flat {
            holders.remove(&account);
        } else {
            holders.insert(account);
        }
        Ok(())
    }

    /// Move a fee between an account and the exchange. Positive `amount` is
    /// charged to the account, negative is a rebate paid to it. The money goes
    /// to (or comes from) the insurance fund when `to_insurance`, else the fee
    /// pool. Neither may go negative.
    pub(super) fn charge_fee(
        &mut self,
        account: AccountId,
        amount: i128,
        to_insurance: bool,
    ) -> Result<()> {
        if amount == 0 {
            return Ok(());
        }
        let acct = self.acct_mut(account)?;
        acct.collateral = acct.collateral.checked_sub(amount).ok_or(Error::Overflow)?;
        let pool = if to_insurance {
            &mut self.insurance
        } else {
            &mut self.fee_pool
        };
        *pool = pool.checked_add(amount).ok_or(Error::Overflow)?;
        if *pool < 0 {
            return Err(Error::Internal("fee pool went negative"));
        }
        Ok(())
    }

    /// Queue an account for a maintenance-margin check if it has positions.
    pub(super) fn mark_dirty(&mut self, account: AccountId) {
        if self
            .accounts
            .get(&account)
            .is_some_and(|a| !a.positions.is_empty())
        {
            self.pending_liq.insert(account);
        }
    }
}
